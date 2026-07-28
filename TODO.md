# TODO

Ordered roughly by what blocks what. Items marked **blocked** have something
concrete in the way, named.

## Solver

- [x] ~~Fix the phase-one bug in `gridwright-simplex`.~~ **Done.** The seeded
      basis is `diag(artificial_sign)`, since each artificial takes a sign so it
      starts non-negative, but `inv` was left as the identity. Every row with a
      positive residual therefore had its value negated, and the very first
      basis was infeasible. Small test problems happened to have residuals all
      of one sign, which is why 21 tests passed over it. Found by asserting
      basic feasibility after every pivot and looking at the first failure.
      Now agrees with HiGHS on all five IEEE networks, on all 118 nodal prices
      in case118, and on mixed models at 1, 4, 12 and 24 hours.
- [x] ~~**Sparse LU**, replacing the dense `m × m` basis inverse.~~ **Done**, as
      a sparse LU factorisation with product-form updates. Measured on the same
      ladder, before and after:

      | Rows | Dense inverse | Factorised | |
      | --- | --- | --- | --- |
      | 216 | 33 ms | 4.4 ms | 7.6× |
      | 432 | 150 ms | 15 ms | 9.9× |
      | 864 | 1.2 s | 57 ms | 21× |

      The exponent falls from about `m^2.7` to `m^1.9`. Where the dense version
      could not finish 2,592 rows inside a ten-minute budget, this does it in
      0.6 s, reaches 3,456 rows in 1.1 s and 20,736 in under two minutes. That
      moves the in-page ceiling from roughly twenty buses over a day to a few
      thousand rows in about a second, which is an interactive model rather than
      an illustration.
- [x] ~~**Forrest-Tomlin updates.**~~ **Measured and not built**, which is the
      cheaper of the two ways to reject something.

      The reason to want it is that every solve lengthens with the pivots since
      the last refactorisation. How much that costs is answerable without
      building anything: vary the interval and watch the total. At 9,216 rows
      the curve is a shallow U: 3.55 s at 32 pivots, 3.30 at 64, 3.16 at 256,
      3.20 at 512, and 5.41 s never refactorising at all. Fitting
      `base + A/k + B·k` gives a base of 3.03 s, and at the optimum the variable
      terms are 0.06 s of refactorisation against 0.07 s of update application.

      Applying the updates is **2.3% of runtime**. That is the entire addressable
      cost, and Forrest-Tomlin would replace it with something rather than
      nothing. The other 96% is triangular solves and pricing, which it does not
      touch. The earlier suspicion that eta growth explained the `m^2.5` tail
      was wrong: that was the factorisation scan, fixed by the symbolic search.

      The measurement paid for itself anyway:
      the default interval moved from
      64 to 256, worth a measured 4%.
- [x] ~~**Faster pricing.**~~ **Built, measured, and left switched off.**

      Having found that Forrest-Tomlin addressed only 2.3%, the obvious next
      move was to price fewer columns, since Dantzig's rule materialises every
      column on every iteration. Partial pricing is implemented and available as
      `price_window`, and across windows of 500, 2,000, 10,000, 50,000 and the
      full count on a 9,216-row model the total ran 3.33, 3.31, 3.24, 3.32 and
      3.30 seconds. Indistinguishable: a cheaper scan buys a worse entering
      variable and the two cancel.

      That is a fact about the shape of these models. An energy system model has
      a few times as many columns as rows, so a scan is a small multiple of a
      solve. Partial pricing earns its keep where columns vastly outnumber rows,
      and the knob is there for a caller whose problem looks like that.

      It also corrects the line above it: naming pricing as the largest cost was
      a guess dressed as a conclusion, and it was wrong.
- [x] ~~**A cost-based crash.**~~ **Done**, and inert on the models this is
      for, which is worth knowing rather than discovering. Each bounded variable
      now starts on whichever bound the objective prefers: the simplex would
      move it there anyway and each move is an iteration. It costs one
      comparison per column and cannot make the starting basis singular, since
      it does not change which variables are basic.

      On an energy model it does nothing at all, because generation costs,
      shedding penalties and capital costs are all non-negative and no variable
      has a bound the objective prefers. It bites on a maximisation, which is
      the same problem with every sign flipped, and there is a test for that.
- [x] ~~**A structural crash.**~~ **Done, and it is the largest single win in
      the solver so far.** Phase one was about three quarters of every solve
      because the starting basis was every artificial variable. A triangular
      selection of structural columns now covers *every* row of the test models,
      and the ladder reads:

      | Rows | Before | After |
      | --- | --- | --- |
      | 3,456 | 0.59 s | 0.27 s |
      | 13,824 | 9.6 s | 5.1 s |
      | 20,736 | 21.0 s | 11.2 s |

      About three times faster, at the same objective, with the differential
      tests against HiGHS unchanged on all six real networks.

      Three bugs on the way, each caught by a network the previous one had
      passed. The substitution ran forwards where the structure is upper
      triangular. The artificials were left as `seed` set them, though crashing
      changes the residuals their signs were chosen against. And the row's
      target was taken as its slack's bound when the running activity already
      carried the slack's term, which counts it twice. That one
      produces plausible values and a basis that factors perfectly well, and surfaces
      thousands of iterations later as a solve that will not converge.
- [ ] **The triangular solves** are what is left after that. Every iteration
      performs one forward and one transposed solve against the factors, and
      nothing about choosing differently avoids them.
- [x] ~~**A fill-reducing ordering.**~~ **Measured and rejected**, twice.
      Greedy minimum degree maintains the column counts as elimination proceeds
      and follows the singleton cascade an LP basis is full of. It **halves the
      fill**: 1,241 nonzeros against 2,450 on an LP-shaped basis. It is also
      **15 to 30% slower end to end**, 26.5 s at 20,736 rows against 21.7 s,
      because the ordering runs on every refactorisation while the fill it saves
      only shortens the triangular solves, and on these matrices the factors are
      small enough either way that halving them saves less than the ordering
      costs. Implemented once with a vector per row and once with flat arrays
      and linked buckets; the second beat the first and still lost. COLAMD would
      face the same arithmetic.

      Worth revisiting only if the factors grow: the test pins the current fill,
      and if it starts rising the trade changes.
- [x] ~~Branch and bound, so the pure-Rust backend can do unit commitment.~~
      **Done.** The relaxation is the same program with integrality dropped, and
      branching is by bounds alone, so a node is a pair of vectors and a
      re-solve with no change to the solver. Depth first, taking the branch
      nearer the relaxation's own choice first, because in an interactive
      setting an answer that arrives beats one that is proved. Returns the
      incumbent and the bound separately and says whether they met.

      Verified against HiGHS on a commitment problem constructed so its relaxation is provably fractional, because many
      commitment relaxations come out integral on their own, and a test built on one of those exercises the
      branching not at all.
- [x] ~~Better branching than most-fractional.~~ **Done**, as pseudo-cost
      branching behind `MipOptions::branching`, and defaulted on.

      The score is the *product* of the two directional estimates rather than
      their sum, because a variable worth splitting is one that hurts both ways:
      a sum happily picks one that is ruinous upwards and free downwards, which
      buys a child that prunes and a child indistinguishable from its parent.
      The cold start takes a prior from the objective coefficient rather than
      strong branching, and the reason is specific to this solver rather than a general preference:
      there is no warm start here, so a probe costs exactly
      what exploring the node costs, and shortlisting ten candidates would spend
      twenty node-solves to save one branch.

      Measured on a unit-commitment ladder, best of five, release build, with
      relaxations provably fractional by construction rather than by assertion:

      | Units × periods | Most-fractional | Pseudo-cost | |
      | --- | --- | --- | --- |
      | 8 × 8 | 468 nodes, 0.43 s | 320, 0.29 s | 1.49× |
      | 10 × 10 | 1,038, 2.09 s | 439, 0.88 s | 2.37× |
      | 12 × 12 | 7,044, 27.2 s | 1,644, 6.3 s | 4.31× |

      The win grows with size and the time column tracks the node column, so the
      scoring is free against a from-scratch simplex solve: there is no size at
      which it is a liability, only sizes at which it is not yet an advantage.
      It loses by 0.91× on one profile, which is what "better on average" looks
      like. At the default `max_nodes` of 5,000, most-fractional runs out of
      budget at 12 × 12 and returns an unproved incumbent, while pseudo-cost
      proves the same answer in 1,644 nodes.
- [x] ~~**A search could report an open gap as proved.**~~ **Fixed**, and it was
      worse than it first looked.

      `proved` was granted whenever the node stack emptied, whatever the bound
      said. The bound itself was recomputed on only one of the five ways a node can
      leave the search (pruned before solving, contradictory bounds, an infeasible
      relaxation, a relaxation no better than the incumbent, or ordinary
      exploration), and the four early exits skipped it. When the last
      open nodes all left by one of those four, the search finished holding a
      bound it had already outgrown.

      On the commitment generator that is not a rounding wrinkle. Every rung
      from 2 × 2 to 6 × 6, under both branching rules, reported `proved` while
      still carrying a gap, the worst of them **7.2%**. Nothing computed was
      wrong (the incumbent was the optimum and the bound was valid), but a caller
      reading `proved` as "this is the optimum" was being told so by a
      search that could not yet know it.

      The per-node work now sits in a labelled block with a single bound update
      after it, so every exit goes through the same code and a sixth cannot
      reintroduce this, and `proved` follows from the gap alone. Knapsacks do
      not reproduce it at all, because commitment problems prune late: the
      incumbent arrives early and the tail of the tree dies to the bound test
      rather than to exploration. The regression test therefore lives beside the
      generator that reproduces it, and it fails on the old code.
- [x] ~~Cuts.~~ **Done**, at the root only, as `MipOptions::cuts`, defaulted to
      Gomory.

      Root-only is what keeps the validity argument short enough to be sure of.
      A tableau row is a linear combination of the constraints and so holds at
      every feasible point, and each nonbasic variable is rewritten as its
      distance from a bound it actually has, so the shifted variables are
      non-negative everywhere rather than only at the current vertex. Nothing in
      the derivation refers to the objective, the incumbent or a branching
      bound, so a root cut is valid over the whole tree with nothing to scope,
      carry or drop. Checked by enumerating all 2^12 binary points of 24 models
      and asserting every point the original problem allows satisfies every cut,
      rather than by argument alone.

      On unit commitment, which is what this engine actually builds:

      | Rung | Off | Gomory | |
      | --- | --- | --- | --- |
      | 8 × 8 | 286 nodes, 263 ms | 15, 22 ms | 11.8× |
      | 10 × 10 | 439, 0.88 s | 26, 75 ms | 11.8× |
      | 12 × 12 | 1,644, 6.45 s | 52, 0.34 s | 19.0× |

      Sixty to ninety-five percent of the root gap closed, and the win grows
      with size. That is larger than the 2 to 4× the branching rule bought on
      the same generator.

      **And they lose on knapsacks**, by up to 3×, with no Gomory cut surviving
      the stability guards at all past 36 items. Two reasons, both worth
      keeping: a knapsack has five rows, so even four cuts nearly double the
      model and every node pays the row count where there is no warm start; and
      the cuts are genuinely weaker there, closing 0.1 to 18% of the root gap
      against 60 to 95. Cover cuts meanwhile find nothing on commitment at all,
      because a row pairing a continuous dispatch variable with a binary status
      is not a knapsack. Both facts are asserted in tests rather than left as
      observations, and the option is an enum of four rather than a boolean so
      that a caller with knapsack-shaped models can say so.

      One measurement error worth recording because it looked like a result: the
      first pass used a per-round floor of four cuts, which put sixteen rows on
      a five-row knapsack and made it three to four times slower at *unchanged
      node count*. Unchanged nodes with worse time is the signature of paying
      for rows rather than of bad cuts, which is what the diagnosis turned on.
- [x] ~~Find where the search stops being usable, and whether cuts move it.~~
      **Done.** `tests/cuts_at_scale.rs`, five commitment rungs against a
      5,000-node budget, ten cells run concurrently, one pass:

      | Units × periods | Binaries | Rows | Cuts off | Gomory |
      | --- | --- | --- | --- | --- |
      | 12 × 12 | 144 | 444 | 91 nodes, 0.45 s | 50, 0.48 s |
      | 16 × 24 | 384 | 1,176 | 204, 6.6 s | 298, 15.1 s |
      | 24 × 24 | 576 | 1,752 | 615, 41.7 s | 193, 23.2 s |
      | 20 × 48 | 960 | 2,928 | **5,000, OPEN** | 1,466, 421 s |
      | 30 × 48 | 1,440 | 4,368 | **5,000, OPEN** | **5,000, OPEN** |

      **Cuts move the ceiling by exactly one rung.** Without them the search
      proves to 576 binaries and gives up at 960; with them it proves at 960 and
      gives up at 1,440. That is the answer the test was built for and it is a
      modest one: cuts buy about 1.7× in problem size, not an order of
      magnitude. A day-ahead commitment of thirty units over forty-eight periods
      is past what this search can prove either way.

      **And cuts lose at 16 × 24** — 298 nodes against 204 and 15.1 s against
      6.6, so more nodes *and* more time. Worth keeping beside the knapsack
      result, because the story until now was that Gomory cuts help on
      commitment and lose on knapsacks. They also lose on some commitment rungs,
      and nothing in the root-gap figures says which.

      Times are taken with every cell running at once and are upper bounds by
      about 1.3 to 1.7×, rising with model size; node counts are deterministic
      and unaffected. Peak memory for the whole concurrent ladder is 56.5 MB,
      which is the measurement that refused the node-representation change
      below.
- [ ] **A dual simplex, and the warm start it unlocks.** The largest single win
      available to the search, and it is arithmetic rather than taste.

      Branch and bound re-solves a slightly changed relaxation at every node,
      and this solver has no warm start at all: `solve` builds a tableau,
      crashes a basis, runs phase one and then phase two, every time. For a
      child node phase one is entirely wasted, because the parent's basis was
      already feasible for everything except the one bound that changed.

      **How much is wasted was an estimate and is now a measurement, and the
      real number is worse.** This entry said "about three quarters", from
      33,670 of 45,205 iterations at 20,736 rows — a synthetic ring. On
      `case1354_pegase`, a real European network of 3,345 rows, it is
      **2,509 of 2,871 iterations, 87.4%**, in a 559 ms solve. Measured
      directly once the counts stopped being discarded at the worker boundary.

      So on a real network nearly nine tenths of every solve is rediscovering
      feasibility that a warm start would have preserved. That moves the
      dual-simplex case from plausible to quantified.

      The reason it is missing is structural rather than an oversight.
      Tightening a bound leaves the parent basis dual feasible and primal
      infeasible, and recovering from that cheaply is precisely what a dual
      simplex does. This solver is primal only, and `Solution` does not return
      the basis, so a warm start is not reachable through the API as it stands.

      What its absence costs, from the node costs `MipOptions` already records:
      a node is a whole cold solve, so node cost follows the LP's own
      `rows^1.8`. At 4,368 rows that is about 248 ms, where a warm-started
      re-solve is a handful of dual pivots. That is the difference between a
      ladder that finishes in minutes and the one that had to be killed after
      three hours in July 2026 with a projected finish of over a day.

      It would also reopen strong branching, which `MipOptions` rejects for
      exactly this reason: a probe costs a full node solve here, so
      shortlisting ten candidates spends twenty node-solves to save one.
- [x] ~~**Stop copying both bound vectors into every open node.**~~ **Measured
      and rejected**, and the reasoning that proposed it was wrong.

      `mip.rs`'s `Node` does hold `lower: Vec<f64>` and `upper: Vec<f64>` in
      full, which is `2 * n_cols * 8` bytes per open node — 70 KB at the 4,368
      columns of a 30 x 48 commitment. The argument for replacing that with a
      `(col, bound, direction)` path was that depth-first pops one node and
      pushes two, so the open set grows with the search and a few thousand open
      nodes would be gigabytes.

      **It does not grow like that.** The whole ten-cell ladder, five rungs
      against two cut settings running concurrently to a 5,000-node budget,
      peaks at **56.5 MB resident** — a 47.4 MB footprint. Not gigabytes, and
      not a problem.

      The flaw in the argument is that pushing two children only happens for a
      node that branches. A node that is pruned by the incumbent, found
      integral, or found infeasible pushes nothing, and in a depth-first search
      with a working incumbent that is most of them. So the open set tracks the
      *depth* of the tree rather than the number of nodes explored, which is
      tens of entries rather than thousands. 70 KB times tens of entries times
      ten concurrent cells is the 56 MB observed.

      Worth keeping as a record of the shape of the mistake: the per-node cost
      was right, the multiplier was assumed, and assuming a multiplier is how
      you get an answer three orders of magnitude out. It would become real if
      the search ever went best-first, where the open set genuinely is O(nodes),
      so the arithmetic above is the thing to re-run if the strategy changes.
- [ ] Cuts at nodes rather than only at the root, and the families neither
      Gomory nor cover touches. Commitment models are dominated by rows pairing
      a continuous dispatch variable with a binary status, which is exactly
      where a flow cover separator applies and where neither family here does
      anything. Lifted cover cuts are the other obvious gap.
- [ ] Consider upstreaming a `row_duals()` accessor to microlp regardless. They
      already compute `y = B⁻ᵀc_B` in `recalc_obj_coeffs` and discard it, and the
      whole ecosystem would benefit.

## Emissions and footprinting

- [x] ~~Carbon intensity of consumption, per bus per snapshot.~~ **Done.**
      Production is what was emitted inside a region; consumption is what was
      emitted on behalf of the electricity it used, which needs flows traced
      back to their source. Because an importer may be re-exporting, that is a
      linear system per snapshot rather than a single pass. Solved under the
      proportional-sharing convention, with buses nothing reached reported
      rather than filled with a flattering zero.
- [x] ~~Average vs marginal emissions.~~ **Done**, computed separately and
      named separately. They differ by several times in the test case, which is
      the whole reason for reporting both.
- [x] ~~Embodied emissions.~~ **Done.** Charged against capacity *added*, not
      against the fleet that already exists, since that carbon is already in the
      air. Counts against the CO₂ budget, so a model cannot build its way under
      a cap for free.
- [x] ~~Emissions by country and by carrier.~~ **Done**, with generation
      alongside the emissions, since the ratio is what a fuel-mix chart shows
      and a fleet intensity is not any one unit's figure.
- [x] ~~Carbon price as an alternative to a cap.~~ **Done**, as
      `Network.co2_price` entering the objective alongside marginal cost.
- [x] ~~Water use and land use, on the same machinery as CO₂.~~ **Done**, and
      literally the same machinery: all three are one row with different
      coefficients, so they share a builder and a fix to how weights or capacity
      blocks are handled lands in all of them.

      Water accrues per megawatt-hour generated, which is what makes it bind in exactly the weeks demand peaks. In much of the
      world, cooling water rather than carbon decides whether a station runs through a dry summer. Land
      accrues per megawatt built, and binds against renewables rather than for
      them, since a wind farm's footprint is what limits how much of it a region
      will accept. Charged on capacity added, not on the existing fleet, whose
      land is already taken.
- [x] ~~Emissions of the *network* itself.~~ **Done.** Consumption accounting
      spreads loss carbon silently over whoever drew power through the lines
      that lost it, which is defensible and hides one of the few numbers a
      transmission planner can act on. Now reported separately, per line and in
      total, as a slice of what was emitted rather than an addition to it. Zero
      means losses were not modelled, which is not the same as their being zero.

## Formulation

- [x] ~~AC power flow via second-order cone relaxation.~~ **Done**, as the Jabr
      relaxation solved with `clarabel`. Reports whether the relaxation came out
      tight, because an inexact one returns voltages that describe no physical
      state and saying so is the only honest option. Two things surfaced while
      building it and both produced infeasibility rather than a wrong number,
      which is the merciful failure mode: power is quoted in MW while impedances
      are per unit, and MATPOWER cases carry transformer tap ratios that change
      what each end of a branch sees.
- [x] ~~Strengthen the relaxation with cycle constraints.~~ **Done for
      triangles**, following Riccardi, Bernardelli and Gualandi
      (arXiv:2604.00664). The arctangent form is nonconvex, but writing
      `W_ij = R + iI = V_i conj(V_j)` turns the cycle identity into
      `Im(W₁W₂W₃) = 0`, a trilinear equality, which McCormick envelopes relax
      convexly. Validity is what the tests check: adding the cuts must never
      lower the bound, since a bound that falls means the cuts are wrong.
- [x] ~~**Head's effect on energy conversion**, as opposed to on available
      capacity.~~ **Done**, both ways, because the two suit different callers.

      A full reservoir yields more megawatt-hours from the same *volume*,
      because the water falls further. Volume drawn per megawatt-hour goes as
      `1/head` and head depends on the level, so unlike the capacity effect this
      one is bilinear and cannot go into the LP as written.

      Piecewise over bands of reservoir level, following Borghetti, D'Ambrosio,
      Lodi and Martello (2008): a binary picks the band, which makes the model
      an integer one and is exact to the band width. And a successive
      approximation that holds head fixed within an iteration and updates it
      under-relaxation, which needs no binaries at all and lands near the same
      answer, with under-relaxation being what stops it oscillating, and a run that
      does not settle says so rather than reporting the last iterate.

      Sixteen tests, including the band head worked by hand, that ignoring
      conversion overstates how far the water goes, and that conversion and
      capacity are separate effects that both apply.
- [x] ~~Spatial branch and bound on the McCormick boxes.~~ **Done.** Over a box
      `R² + I²` lies under its corner secant and `u_i·u_j` lies over its
      McCormick underestimator, so `secant ≥ McCormick` is implied by the
      equality Jabr drops: no feasible point is ever cut off, and both sides
      collapse onto the truth as the box closes. Splitting is then what buys
      tightness. case57's relaxation is only a bound at the root and 33 nodes
      prove its optimum to 4e-11; on case118 the bound climbs and the cone gap
      falls twenty-five fold.
- [x] ~~A small cone gap was being read as "this solution is physical".~~
      **Fixed.** The cone is a per-branch statement and says nothing about
      angles closing around a loop, so a cycle-inconsistent point could be
      reported as optimal. Cycle consistency is now measured from the solution
      and folded into the status.
- [x] ~~**Cycle constraints for fundamental cycles longer than three.**~~
      **Done**, and the objection that stopped it turned out to be an artefact
      of how it was written rather than of the maths.

      Writing the identity out is what does not scale: the imaginary part of a
      product of `k` complex numbers has `2^(k-1)` terms. Building the product
      one factor at a time costs six auxiliary variables per step and `k − 1`
      steps, so the cost grows *linearly* in the length.

      It matters more than it sounds. A five-bus ring is meshed, has exactly one
      cycle, and a triangle-only formulation constrains nothing in it at all.
      IEEE 14 has seven independent cycles and only a few are triangles.

      Cycles come from a spanning forest, so they are a basis of the cycle
      space: constraining them constrains every cycle, since any other is a
      combination and the identity is additive around combinations.
- [x] ~~Bus shunt admittances.~~ **Done.** A conductance draws real power that somebody has to generate
      (case300 carries over a megawatt of it), and a susceptance injects the reactive power capacitor banks exist to supply.
- [x] ~~Transformer phase shift angles.~~ **Done**, and why they had looked
      absent is worth recording: the Jabr cone is invariant under rotation of
      `(R, I)`, so no plain relaxation can distinguish a network with a phase
      shifter from one without. Only the spatial search, narrowing the boxes
      until the cycle envelopes bind, makes the device visible.
- [x] ~~Apparent power limits on lines.~~ **Done**, and the AC model carried no
      thermal limits *at all* before this: it could route as much as the
      impedances allowed. A rating bounds `√(P² + Q²)`, a circle in the complex
      plane, and both components are already linear in the decision variables,
      so it goes in as a three-dimensional second-order cone with no auxiliary
      variables. A square limit would have allowed the corner, √2 times the
      rating, which is what a pair of linear bounds carried over from the DC
      model would give.
- [x] ~~N-1 security constraints.~~ **Done.** Formulated through line outage
      distribution factors, so security costs rows rather than columns: the
      naive approach duplicates every flow variable per contingency, while LODF
      turns each outage into constraints on the base-case flows that already
      exist. Lines whose loss would island the network are reported rather than
      silently skipped. Verified by replaying every outage against the solved
      base case, and on IEEE 14 under full N-1.
- [x] ~~Hydro head effects.~~ **Done for available capacity**, which is the
      part that is linear: power is proportional to the height water falls
      through, so a reservoir near empty cannot reach its rating. Evaluated at
      the *start* of each period, since using the end level makes the constraint
      self-limiting and a brim-full reservoir could never reach its rating.
- [x] ~~Rolling horizon unit commitment.~~ **Done.** Overlapping windows with
      reservoir levels and commitment states carried across, since a window that
      assumes every unit starts cold invents start-up costs already paid.
      Checked against solving the same horizon whole, and that more lookahead
      never costs more.

## Flexible demand

All four ways demand can fail to be served are now distinct, which they were
not: shed at the value of lost load, moved to another snapshot, declined on a
willingness-to-pay curve, or curtailed under contract. They answer different
questions and cost different amounts, and collapsing them into one penalty
was hiding most of the demand side.

- [x] ~~**Shiftable load.**~~ **Done.** A signed deviation from the demand
      profile, bounded by a per-load fraction, summing to zero over a window so
      that what leaves one snapshot arrives in another. That conservation is the
      whole distinction from shedding, which simply deletes the expensive hours.
      Movement is charged in both directions, since a signed variable cannot
      carry a cost on its own and without one the optimiser slides demand
      between equally priced snapshots for no reason.
- [x] ~~**Price-elastic demand**, a load that declines rather than moves.~~
      **Done**, as tranches of a willingness-to-pay curve. Shedding prices
      unserved energy at the value of lost load, a number in the thousands
      chosen to mean "never do this"; a curve turns dropping demand into a
      choice with a price. Tranches are taken cheapest first, so a caller does
      not have to sort the curve, and demand beyond what the curve covers still falls back on the value of lost load:
      a curve says what a consumer will pay, not that they are indifferent past its end.
- [x] ~~**Interruptible contracts.**~~ **Done.** A binary per snapshot saying
      whether the contract was called, the energy not delivered bounded by both
      the contract's size and that binary, and the calls counted over the
      horizon against the agreed number.

      That count is the whole contract: without it this is expensive shedding
      with extra steps, and it is also the only part that cannot be written
      linearly, which is why a contract makes the model an integer one.

## Memory

Peak resident memory on the 256-bus, 8,760-snapshot model is 1.50 GB, down from
1.95 GB. What moved it, and what did not, are both worth writing down.

| | |
| --- | --- |
| Column bounds, costs, integrality | 406 MB |
| Row bounds | 99 MB |
| CSC, as handed to the solver | 415 MB |
| Transpose counters | 65 MB |
| Accounted | 985 MB |
| Measured | 1,504 MB |

**What worked.** The model is now built column major and only column major: the
constraint builders' row batches are transposed as they are absorbed, so the
merged row major matrix never exists. Nothing downstream ever read it (HiGHS takes
      compressed sparse columns, and so does the simplex), so it was 375 MB
spent on a representation with no reader, plus a row offsets array and a second
copy of the row bounds. Measured saving 447 MB, rather more than the 375 MB
predicted.

It also made the model faster, for a reason that had nothing to do with memory:
`to_csc` called the *serial* transpose, so every solve paid about 79 ms for it.
Folding the transpose into the absorb and threading it over the batches, which are already one per builder thread so no
      chunking had to be invented, put model construction at **104 ms including the transpose**, against 94 ms plus a
      separate 79 ms before: 1.7× faster to a solver-ready matrix, and between 1.6×
and 1.9× across 64 to 512 buses.

**What did not work, and why.** Replacing the transpose's `threads × n_cols`
histogram with one atomic counter array cut allocation from 1.8 GB to 65 MB and
moved peak resident memory not at all. Taking ownership of the batches so each
is released as it is merged, rather than holding all of them, likewise moved it
not at all. In both cases the allocator keeps freed pages rather than returning
them, so this metric records what was allocated at the high-water mark rather
than what was live. Both changes were kept regardless: the memory does become
available for reuse, and 65 MB against 1.8 GB matters where address space is not
      free: the WebAssembly target has 4 GB of it in total.

- [ ] Measure against an allocator that returns pages, to find out how much of
      the remaining 519 MB of slack is live and how much is retention. Until
      that is known, the accounting above is a ceiling rather than a description.

## Transpose, measured

The transpose was suspected of costing ~90 ms and being the thing to optimise.
It was neither. Per-phase timing on the real sparsity pattern, 14 threads:
scatter 13.4 ms (48%), scan and cursor reset 4.5 ms, sort 4.2 ms, count 3.0 ms, allocation 3.0 ms,
about 21 ms intact rather than 90. The 90 ms readings were machine
load: with four competing busy loops the same kernel measures 60–150 ms, because
it is five back-to-back full-width parallel regions and each ends when its
slowest worker does. That also explains why three different counting strategies all measured the same:
the measurement was environment-bound, so the algorithm
could not move it.

Ruled out empirically, so nobody repeats them: allocation and zeroing (≤3 ms of
`mmap`, page faults only on the first call in a process), the prefix scan
(4.5 ms), `u32` versus `usize` indices (already `u32`), and the count phase
(3 ms). Cache-blocked radix partitioning pays only in the uniform-random regime,
which this matrix is not in. A random-column matrix of the same shape takes
175 ms against 21 ms for the real one. The scatter moves ~700 MB at ~52 GB/s
against 130–220 GB/s for `memcpy`, so it is 3–4× off bandwidth, not 20×.

- [ ] Pre-fault the output arrays with a parallel sequential touch before the
      scatter. Worth ~10 ms on the first call in a process, where ~29,000 soft
      page faults are currently taken from inside random-access loops; faulting
      the same pages sequentially is 3–5× cheaper. Only the first call, so it is
      worth having for a one-shot CLI run and worth nothing for a server.

## Data formats

Every reader targets the same `Network`, and every one returns a `Case` whose
`notes` say what it had to drop. `load_any` identifies a file from its content
and its name and dispatches, so a caller who has been handed a file rather than
a format does not have to work out which they have.

- [x] ~~CSV directories~~, ~~MATPOWER `.m`~~, ~~PSS/E RAW v29–v35~~ including
      two- and three-winding transformers, all three winding-data conventions,
      both impedance bases and two-terminal DC.
- [x] ~~PowerModels JSON~~ with its per-unit convention, ~~lossless native
      JSON~~ for round-tripping a network to a browser and back.
- [x] ~~Parquet~~, tables and wide time series, the latter staying numeric end
      to end because that is the whole reason for the format.
- [x] ~~Spreadsheets~~ (`.xlsx`, `.xls`, `.xlsb`, `.ods`), which is how much of
      the world publishes energy statistics.
- [x] ~~PyPSA netCDF~~ through a pure-Rust HDF5 reader, so the WebAssembly
      target keeps working. Converts ohms to per unit, which is the trap.
- [x] ~~CIM/CGMES RDF/XML~~, one file or a directory of profiles, joining
      equipment to buses through terminals as CIM requires.

Still missing:

- [x] ~~**CGMES in its published form**, a zip of profile files.~~ **Done**, and
      it recurses one level, since a pan-European model is an archive of
      archives. Both formats that are zips, a spreadsheet and a CGMES model,
      are told apart by what is inside rather than by the extension, because a
      CGMES archive is as likely to be named for its operator as for its
      contents.
- [x] ~~**The SSH profile** of a CGMES model.~~ **Done.** The equipment profile
      describes plant and the hypothesis says what it is doing, so a reader that
      stops at equipment produces a network with correct topology and no load in
      it, which solves and means nothing. Set points and in-service flags are
      both honoured, and a model built as designed rather than as operated is a
      different and usually more capable network.
- [x] ~~**The SV profile**, which carries a solved state.~~ **Done.** Voltages,
      angles, branch and machine flows, tap positions and shunt sections, read
      through `load_model_with_state`.

      Returned beside the `Case` rather than folded into the `Network`, because
      the value of it is having a second answer to compare against rather than a first one to trust:
      it is what the operator's own tools produced. Every
      entry is an `Option`, since a published state is routinely partial and a
      zero would be a claim the model never made.

      CIM's into-the-equipment sign is kept rather than flipped to this
      project's generator convention, which leaves every node summing to zero
      available as a free check on the reader. Angles arrive in degrees and are
      stored in radians, as the MATPOWER reader already does for phase shifts.
- [x] ~~**PSS/E `.rawx`**, the JSON reformulation v35 introduced.~~ **Done**,
      and it is a fraction of the length of the RAW reader for a reason: RAWX
      names every field where RAW is positional, so the whole class of
      version-dependent column offsets simply does not arise. Three JSON
      dialects now share the extension and are told apart by content.
- [x] ~~**UCTE-DEF and IEEE Common Data Format.**~~ **Done**, both unconditional
      since neither needs a dependency.

      Fixed width means columns rather than whitespace, because a blank field
      that shifts every later field produces a network that parses cleanly and
      is wrong. Both readers cut by position and there is a test in each for
      exactly that.

      What made them worth the effort is the conventions, each derived in a
      comment and re-derived in a test. **IEEE CDF** quotes impedances, line
      charging and bus shunts *already per unit* on the title card's base,
      the opposite of MATPOWER, where the shunts are MW/MVAr and must be divided
      by it, so a reader that treated them alike is out by the base. A blank
      rating means unlimited rather than unusable, and a blank turns ratio means
      one rather than zero. **UCTE-DEF** quotes ohms and microsiemens, and the
      admittance base is the *reciprocal* of the impedance base, so susceptance
      multiplies where impedance divides: dividing gives 2.1e-10 instead of
      0.4332. Current limits are amps and become MVA through √3·V·I, the same
      conversion CIM needed, but a transformer's rating is already MVA and must
      not go through it. Transformer impedance is referred to the regulated
      winding, so a 380/110 read against the wrong end is out by twelve. Nominal
      voltage is the seventh character of the node code rather than a field, and
      the table is not monotonic. Generation is negative in the file, and the
      sign flip swaps the ends of the band.

      The IEEE 14-bus in CDF form joins the corpus that reads the same network
      to the same answer in every encoding: 14 buses, 20 lines, 259 MW, matching
      MATPOWER, PSS/E, RAWX, netCDF and Excel.

      Two things left open. The UCTE angle-regulation *sign* could not be
      verified against a reference and is stated as an assumption in the code
      rather than as a fact. And UCTE country letters are not expanded to ISO
      names, because the table could not be verified and a wrong one mislabels a
      whole country.
- [x] ~~**Writers for MATPOWER and PSS/E.**~~ **Done.** Each returns the notes
      describing what the format could not hold, in the same way every reader
      does, because a writer that silently dropped storage would produce a file
      someone trusted. Verified by reading back with the readers that are
      themselves cross-validated against other encodings of the same network.

      Writing them turned up a reader bug worth having found: a lone `0` at the
      head of a record ends a section in RAW, and a transformer with no
      resistance begins with one. PSS/E's own writer emits `0.00000` and so
      never hits it, which is exactly the kind of thing that stays hidden until
      a file arrives from somewhere else.
- [x] ~~**A writer PyPSA can read.**~~ **Done**, as PyPSA's own CSV dialect
      rather than its netCDF, and the reason is worth recording rather than
      leaving as "harder".

      A conformant netCDF4 file needs HDF5 dimension scales, which means
      `DIMENSION_LIST` and `REFERENCE_LIST` attributes carrying object references,
      file offsets not known until the file has been laid out. The
      pure-Rust HDF5 library exposes the reference datatype and no way to emit
      one, and a `.nc` xarray refuses to open is worse than no `.nc`.
      `import_from_csv_folder` reaches the same destination by a road that
      exists.

      The conversion that matters runs the opposite way from the reader's: PyPSA
      states impedance in ohms, so per unit has to be undone against the base
      voltage. Writing per-unit values into a field PyPSA reads as ohms gives
      lines that are very nearly short circuits, which does not fail:
      it produces answers.
- [ ] **A CIM writer.** Still wanted and still last: a CGMES file that is not
      conformant is worse than none, and conformance is a large surface.
- [x] ~~Reading with no filesystem.~~ **Done.** `load_bytes` takes a name and a
      buffer; `load_files` takes a set, which is what a picker or a dropped
      folder gives and the only way to express a CSV directory or a CGMES model
      split across profiles. The whole layer builds for
      `wasm32-unknown-unknown`, which is what the planned interface needs.
- [ ] **Streaming the large time series.** A year of hourly data for a
      continental model is read whole into memory. Parquet is chunked on disk
      and the reader already walks it in batches, so bounding memory is a matter
      of not accumulating, not of new machinery.

## Interface

The library side is done: the engine, the format layer and the pure-Rust solver
all build for `wasm32-unknown-unknown`, every reader takes bytes rather than a
path, and a network round-trips losslessly through JSON.

What this section is now for: building an actual **simulation studio** — a
design tool in the sense that Blender or Grasshopper are design tools, where the
model is something you shape and interrogate rather than a file you submit to a
batch job. The engine is fast enough to make that possible and nothing has yet
been built to prove it. The plan below is staged so that each stage produces
something usable rather than something half-built, and so that the risky parts
are proved before the expensive parts are started.

The measurements that decide the architecture come first, because every choice
after them is downstream of what the browser can actually do.

- [x] ~~**Sparse LU first.**~~ **Done**, and it moved the ceiling this section
      was written around. 864 rows took a second when this item was written; it
      takes 57 ms now, 3,456 rows take 0.27 s and 20,736 take 11.2 s. The
      structural crash basis is most of the second factor and the sparse
      factorisation the first.

      So the honest statement of the browser constraint has changed. It is no
      longer "a page can barely solve twenty buses over a day": a few thousand
      rows is interactive in-page, which is a small national model at daily
      resolution or a regional one at hourly. What it still is not is a
      continental year, and no amount of solver work in this crate makes it one. That is the
      decomposition question, not the factorisation question.
- [x] ~~**Decide where the solve happens.**~~ **Decided, on measurement rather
      than on taste: everything runs in the page.** No server, static hosting.

      The decision was blocked for months on not knowing the in-page ceiling.
      It is now measured, in the actual target rather than extrapolated from
      native numbers. See *What the browser target actually costs* below.

### A third dimension is affordable, and it is not a renderer

Measured against vendored egui/eframe/epaint 0.35 sources and real benchmark
crates, not recalled. Numbers are native Apple Silicon, release, single
threaded, at 2x pixels-per-point.

**The recommendation is software axonometric projection into a cached
`Arc<Mesh>`, drawn with the `egui::Painter` already in use.** No second
renderer, no shader-version matrix, no new dependency, and behaviour identical
between native and web.

| what | cost |
| --- | --- |
| project 132,000 vertices, 3D to 2D affine | 0.093 ms |
| depth-sort 33,000 faces and rebuild 198,000 indices | 0.534 ms |
| re-emit the cached `Arc<Mesh>` (clone only) | ~0 ms |
| **static camera, full 13k-bus network** | **0.10 ms/frame, 1 draw call** |
| **camera moving** | **0.73 ms/frame, 1 draw call** |
| the naive alternative: 33k individual shapes | 1.75 ms/frame |

Budget wasm at 2-4x native, so roughly 2-3 ms on a camera-change frame. That is
comfortable inside a 16 ms budget for a network larger than anything the solver
can handle in a tab.

`Shape::Mesh` holds an `Arc<Mesh>`, which is what makes the caching free: build
the projected mesh when the camera moves, clone the `Arc` every other frame.

**Two measured cliffs to stay off.**

- **Feathering is the dominant cost.** Anti-aliasing roughly triples the
  triangle count on strokes: 20k line segments tessellate in 2.22 ms with it and
  0.63 ms without. `ctx.tessellation_options_mut(|o| o.feathering = false)` for
  bulk geometry is a 3.5x win.
- **Circle radius has a sharp threshold.** epaint serves small discs from a
  prerasterized atlas as four-vertex quads and falls back to path tessellation
  above that: 13,000 circles cost 0.53 ms at r=3 and **5.77 ms and 17 MB of
  vertex buffer** at r=9. Bus glyphs stay small, or get drawn as our own quads.

**Ordering is exactly the guarantee a painter's algorithm needs.** `PaintList`
is a `Vec<ClippedShape>` and the tessellator documents that shapes are
tessellated in the order given, so within a layer emission order *is* z-order.
`Painter::add` returns a `ShapeIdx` and `Painter::set` overwrites in place
preserving position — so slots can be reserved and filled later, but nothing can
be reordered after emission. Sort before emitting.

**Text can rotate, and it is free.** `TextShape` has `pub angle: f32` and a
`.with_angle()` builder; 2,000 labels cost 0.14 ms rotated or not. What is not
available is *shear* — `TSTransform` is uniform scale plus translation only — so
true axonometric type sheared into the ground plane is out. That is the right
answer anyway: sheared type is unreadable, and every real map billboards its
labels.

**The painter's-algorithm caveat, which decides this.** A per-face depth sort is
*exact* only for geometry monotone in depth. A ground plane with vertical
extrusions is; interpenetrating geometry is not, and with 20k line segments
there will be cyclic overlaps no sort can order correctly. **Confirm the layout
is depth-monotone before committing to this**, because it is the one thing the
GL path would fix.

#### The escape hatch, and the trap in it

`egui_glow::CallbackFn` is there if per-pixel depth, transparency sorting or
>100k elements are ever needed. Three failure modes, all silent or confusing:

1. **The depth buffer is inverted from the obvious assumption.** On web,
   eframe requests the WebGL2 context with no attributes dictionary, so the
   Khronos default `depth = true` applies and a depth buffer is there — while
   `WebOptions::depth_buffer` is inert and documents itself as unused. Natively,
   `NativeOptions::depth_buffer` defaults to **0**, so there is **no depth
   attachment at all**, and `glEnable(GL_DEPTH_TEST)` plus
   `glClear(GL_DEPTH_BUFFER_BIT)` then **succeed with no GL error and do
   nothing**. Web works and native silently does not. Set `depth_buffer: 24` and
   assert the attachment at startup rather than trusting it.
2. **`WebGlContextOption` defaults to `BestFirst`**, which can land on WebGL1,
   where `#version 300 es` will not compile. Force `WebGl2`, and take the
   `#version` line from `ShaderVersion::get(gl).version_declaration()` rather
   than from a `cfg!(target_arch = "wasm32")` guess.
3. **Capturing an `Arc<glow::Context>` in the callback compiles natively and
   fails on wasm.** On wasm `glow::Context` holds `RefCell<SlotMap<..>>` and is
   `!Send + !Sync`, while `CallbackFn::new` demands both. Take the context from
   `painter.gl()` *inside* the closure and capture only the handles, which are
   plain `Copy` keys on both targets.

egui restores its own state after every callback and never clears depth; the
scissor rect is still set, so clearing depth inside the callback is correctly
confined to the widget.

#### What was ruled out

- **`three-d` 0.19** is genuinely compatible — it re-exports the same glow 0.17,
  and `Context::from_gl_context` exists to adopt a foreign context. Verified by
  building it against eframe 0.35 for both targets. Only worth it for a real
  scene graph and lighting; its `Context` is also `!Send` on wasm and has to
  live in a `thread_local!`, and `RenderTarget::screen` binds framebuffer 0 so
  clears need scoping to the viewport.
- **`rerun`/`re_renderer` and `bevy_egui`** are wgpu-only and would fight eframe
  for the context.
- **`egui_plot`** has no 3D at all.
- **`transform-gizmo-egui`** is architecturally ideal — backend-agnostic, CPU to
  `epaint::Mesh` — and pinned to egui 0.34. Revisit after it bumps.

### What the browser target actually costs

Measured 26 July 2026, warm, n=5, identical code compiled twice. These numbers
decide the architecture, so they are recorded before the plan that rests on
them.

**The solver costs 1.3× under wasm and nothing else.**

| Size | Native | wasm | Penalty |
| --- | --- | --- | --- |
| 8 × 24 | 8.1 ms | 10.0 ms | 1.23× |
| 16 × 24 | 37.3 ms | 48.5 ms | 1.30× |
| 24 × 48 | 300.3 ms | 399.9 ms | 1.33× |
| 32 × 48 | 568.5 ms | 750.3 ms | 1.32× |
| 48 × 72 | 2.98 s | 3.86 s | 1.30× |
| 64 × 96 | 11.34 s | 14.37 s | 1.27× |

1.27 to 1.33× across three orders of magnitude, which is about as good as wasm
gets on tight numeric code. The reason it is so flat: the simplex does not use
rayon, so it loses no parallelism by moving to a single-threaded target. It was
already single-threaded.

**Construction costs 2 to 5×, and it does not matter.**

| Size | Native | wasm | Penalty |
| --- | --- | --- | --- |
| 256 × 168 | 2.4 ms | 5.0 ms | 2.1× |
| 256 × 720 | 6.2 ms | 21.7 ms | 3.5× |
| 256 × 2190 | 16.5 ms | 65.8 ms | 4.0× |
| 256 × 8760 | 50.5 ms | 265.6 ms | 5.3× |

The penalty grows with size because construction *does* use rayon — twelve
`par_iter` sites in `gridwright-build` plus the CSC transpose — and loses that
parallelism. But construction is roughly fifty times cheaper than the solve at
interactive sizes: 5 ms of build against 400 ms of solve at 256 × 168.

**rayon does not trap on `wasm32-unknown-unknown`.** It falls back to the
calling thread. This was the risk that looked like it might sink the whole
plan; it is not one. The module has zero host imports and the output is
numerically identical to native — 559,104 nonzeros at 256 × 168, matching the
published table exactly.

**A full year fits.** 256 buses over 8,760 snapshots — 16.3M variables, 29.2M
nonzeros — assembles inside a wasm32 module in 266 ms. The memory claim this
project rests on now holds in the target it was always about.

**Threads compile, if we ever want them.** The whole dependency tree builds with
`+atomics` and `--shared-memory` on nightly with `-Z build-std`, and
`memory.buffer instanceof SharedArrayBuffer` is true. (`+bulk-memory` and
`+mutable-globals` have been on by default since Rust 1.87; passing them is
harmless and redundant.) So the door is open, and it is deliberately not being
walked through.

The first reason is the arithmetic already given: threads buy 2–5× on
construction, the component that is fifty times cheaper than the solve. But the
second reason is the state of the toolchain, which was researched rather than
assumed and is worse than expected:

- **`wasm-bindgen-rayon` 1.3.0 was published December 2024** and has had two
  commits since, both unreleased. Adopting it means pinning a git revision, not
  a version.
- **The documented setup does not currently work.** Its README flags, plus
  released `wasm-bindgen` 0.2.126, plus current nightly, fail at
  `wasm-bindgen --target web` with *"failed to find `__heap_base` for injecting
  thread id"* — a regression from `nightly-2026-05-07` where dlmalloc began
  using the linker heap range that wasm-bindgen parks bookkeeping in. Exporting
  `__heap_base` gets past the CLI error but reportedly may still stall during
  pool bootstrap. The fix is merged and unreleased. Adoption means pinning a
  nightly *and* a wasm-bindgen git rev *and* a wasm-bindgen-rayon git rev, and
  re-validating that triple on every bump.
- **`-Z build-std` is not stabilising soon.** It is an accepted Rust 2026
  project goal whose objective for this cycle is "start implementation".
- **The payoff is 1.5–3×, not Nx.** That is Squoosh's measured range across
  codecs, and the crate author's own demo shows 3.14×. Safari hard-caps
  `navigator.hardwareConcurrency` at 8 regardless of hardware, and under
  privacy protection returns a *random* 1–64, so it must never be passed
  straight to `initThreadPool`.
- Two open correctness bugs with no fix: a WebKit-only out-of-bounds trap in
  pool construction, and a rare `Atomics.wait` throw from `crossbeam-channel`
  during init on the main thread.

**One thing to record now in case threads are ever adopted**, because it is a
property of our code rather than of the ecosystem, and it is the kind of fault
that produces a silently sequential build rather than an error:

`gridwright-model/src/csc.rs:185` and `:287` call `rayon::current_num_threads()`,
which **initialises rayon's global pool on first touch**. `wasm-bindgen-rayon`
installs its worker pool through `build_global()`, which fails if the registry
already exists. So any path reaching those two lines before `initThreadPool`
resolves would install the single-threaded fallback, make `initThreadPool`
throw, and ship a sequential build that looks threaded. The fix is to move both
calls behind the init boundary.

Otherwise the shape is unusually favourable: 25 rayon sites across
`gridwright-build` and `gridwright-model`, every one a `par_iter` variant, and
**zero** `ThreadPoolBuilder`, `rayon::scope` or `rayon::join`. Since
`build_global` installs a process-wide registry, none of those call sites would
need modifying. No fork required — one ordering fix and an audit.

**Therefore, the interactive budget in-page today, no threads, no HiGHS:**

| Feel | Size |
| --- | --- |
| Instant, under 100 ms | ~1,000 rows |
| Responsive, under 500 ms | ~2,600 rows |
| Noticeable, under 1 s | ~3,500 rows |
| Needs a worker and a progress bar | 7,800 rows → 3.9 s |

A regional network at daily resolution, or a small system hourly, is
comfortable in-page with the pure-Rust simplex alone.

### HiGHS *is* available in the browser, and it moves that ceiling ~20×

This was researched expecting to rule it out, and it ruled itself in.

[`highs-js`](https://github.com/lovasoa/highs-js) is an Emscripten build of
HiGHS, **MIT-licensed**, and unusually alive: npm `1.15.2` published
2026-07-22, `1.15.3-pre.3` on 2026-07-24, upstream tracked within weeks.
(The other package this project previously cited, `fuglede/highs-wasm`, is
**dead — the repository 404s**, and its stale search-engine description is what
makes it look otherwise. Verified.)

Measured rather than assumed:

| Property | Finding |
| --- | --- |
| Speed vs native HiGHS | **1.10–1.38×** on 240k and 960k nonzero LPs, identical iteration counts and objectives |
| Duals | **Returns every row dual.** 319,980 of them on the larger case |
| Threading | **Single-threaded** — so **no COOP/COEP, no cross-origin isolation** |
| Size | 3.4 MB raw, **826 KB brotli** |
| Marshalling a 10M-nonzero CSC matrix between two wasm heaps | **~2 ms** via `TypedArray.set` |
| Real ceiling | Emscripten's **2 GiB heap**, reached around 2.5–3M nonzeros |

The prerelease line exposes the **full low-level C API** — `createModel` taking
CSC directly, warm starts from a saved basis, ranging, and `getIis`. It also
ships **PDLP**, so first-order methods can be tested against the biggest models
for free.

**A correction to an earlier version of this entry**, which said `getIis` gave
us "half of the Stage 4 infeasibility work for free". That was too optimistic.
HiGHS's own documentation says the feature "can only be used for LPs" and is
"under development… not as robust or efficient as it will be", its tracking
issue is still open, and segfaults and empty-output bugs were reported through
late 2025 and early 2026. It is a second-tier tool, not the foundation. The
foundation is priced slack — see Stage 4.

One trap worth recording: the *stable* API is one-shot and takes CPLEX LP
**text**. Serialising 2M nonzeros to that format took 245 ms and produced a
26.9 MB string, against 1 ms for the equivalent CSC copy. Use the CSC path,
never the text path.

**So the plan is both solvers, chosen by size.** HiGHS-js for anything up to a
few million nonzeros — which covers essentially every model a person will
interact with — and the pure-Rust simplex above that, where the 2 GiB
Emscripten heap gives out, and as the zero-dependency path when a second module
is unavailable.

**Keeping our own simplex is not sentiment.** The pure-Rust field was surveyed
and there is no replacement: `microlp` returns **no duals at all** (verified by
reading its `Solution` type), which disqualifies it and everything downstream of
it in `good_lp` for a tool whose output is nodal prices. `clarabel` returns
duals but is interior-point with **no crossover**, so on the massively
degenerate LPs that network dispatch produces it converges to an analytic-centre
dual rather than a basic one — legitimate, but not the numbers power-systems
tooling expects for locational marginal prices. Everything else is unmaintained
or toy-scale. Ours remains the only pure-Rust solver here that returns the duals
this project exists to produce.

### The architecture this settles on

```
crates/
  gridwright-*        the engine, unchanged, no UI dependencies
  gridwright-studio/  eframe app: docking, network editor, charts, 3D view
  gridwright-worker/  [[bin]] -> its own .wasm, wraps the engine,
                      receives a model, streams progress, returns results
                      + loads highs-js as a sibling module and picks a
                        solver by problem size
```

Four decisions, each with a reason rather than a preference:

**egui/eframe for the shell.** The only Rust option where dense technical
tooling, static hosting, and a genuine native desktop build from one codebase
are simultaneously true. Rerun's viewer is the proof point: egui/eframe, shipped
as static `.html` + `.wasm` + `.js`, and it explicitly avoids spawning threads
under wasm — the same constraint. The ecosystem is unusually well matched:
`egui-snarl` for the node editor, `egui_tiles` for docking (what Rerun uses),
`egui_plot` for charts, `egui_graphs` for large-graph layout, `wgpu` embedded in
a panel via render callback for the 3D view.

Ruled out with reasons rather than vibes. **Bevy**: no wasm multithreading, the
tracking issue has been open since 2022, and WebGL2 versus WebGPU needs two
separate builds. **Leptos**: its entire value is SSR and server functions, which
"no server" deletes, and desktop is a Tauri wrapper documented against a version
two majors old. **Slint**: its own docs call the web target demonstration-grade,
and the tri-licence interacts badly with this project's AGPL/commercial posture.
**Dioxus** stays the runner-up — better if HTML accessibility and CSS matter
more than diagram quality, but the diagram surface would be a hand-rolled canvas
either way, which is two rendering models to maintain.

**Compute in a dedicated Web Worker, as a second wasm module.** This is the
decision that removes almost all the risk. A worker holding its own
single-threaded wasm instance keeps the UI at 60 Hz through a fourteen-second
solve, and it needs **stable Rust, no nightly, no `-Z build-std`, and no
cross-origin isolation**. The threading machinery is only required for
*multi-core speedup inside* the solve, which the measurements say is not worth
buying. Splitting these two concerns is what lets v1 ship on stable.

**The solver swaps by target, not by fork.** `highs-sys` is now declared under
`[target.'cfg(not(target_family = "wasm"))'.dependencies]`, so a wasm build
succeeds with default features on and the browser silently gets the pure-Rust
simplex while native gets HiGHS. One codebase, two capabilities.

**Where a dependency cannot follow us into the browser, we write the
replacement.** This project already did it once — the from-scratch simplex
exists because HiGHS could not go where the engine needed to go, and it turned
out to solve a case HiGHS declines. That is the standing policy for this work
rather than a last resort: a crate that will not compile to wasm is a
specification for one that will, not a reason to add a server.

### Stage 0 — foundations, before any pixels

**Three engine changes block everything, and all three were verified in the
source rather than assumed.** Each one is cheap now and a migration later.

- [ ] **`Bus` has no coordinates.** Verified: `gridwright-net/src/lib.rs:210`
      has `name`, `country`, `synchronous_area`, shunt terms — and no `x`, `y`,
      `lat` or `lon`. Stage 3 below says "geographic layout when coordinates
      exist"; they do not exist. Adding a field to a serialised type after
      people have saved files is a migration, so add it before the first save
      format ships.
- [ ] **`Network` indexes by position, which breaks the moment editing lands.**
      Verified: `buses: Vec<Bus>`, `lines: Vec<Line>` and the rest, all
      referenced by `usize`. Deleting one bus renumbers every bus after it,
      which simultaneously invalidates undo commands, the current selection,
      any probes, and the mapping from solver rows back to components.
      Generational handles need to exist **before the first edit operation is
      written**, not after the first bug.
- [ ] **`Solver::solve()` cannot report progress or be cancelled.** Verified:
      `gridwright-solve/src/lib.rs:193` is
      `fn solve(&self, lopf: &Lopf) -> Result<Solution, SolveError>` — one
      blocking call, no hooks. The simplex already tracks `iterations` and
      `phase_one_iterations` internally and throws them away; branch and bound
      already tracks nodes, incumbent and bound. The data exists; there is
      nowhere to put it.

      This is a **solver decision, not a UI decision**, and it compounds with
      the `panic = "abort"` constraint recorded below: cancellation has to be a
      `Result` variant threaded through the loop. Both have to be settled before
      the loop is written.

- [ ] **CI, first, because everything else rests on it.** There is currently no
      `.github/workflows` at all, and nothing guards the wasm target. It works
      today because it was checked by hand. A workflow that runs the 644-test
      suite, `cargo build --target wasm32-unknown-unknown`, `clippy` and
      `fmt` is an hour's work and protects the foundation the entire interface
      plan stands on.
- [ ] **`gridwright-worker`**: the compute module. A `[[bin]]` compiled to its
      own wasm, exposing `load(bytes) -> Model`, `build(Model) -> Problem`,
      `solve(Problem) -> Solution`, with progress posted mid-solve. Native build
      of the same crate runs the same calls directly on a thread.
- [ ] **One `SolveBackend` trait, the only place the two targets diverge.**
      Worker implementation for web, `std::thread` for native. If anything else
      in the UI needs `cfg(target_arch)`, the abstraction is in the wrong place.
- [ ] **`highs-js` as a sibling module inside the worker**, selected by problem
      size, with the pure-Rust simplex as the fallback above the 2 GiB heap and
      whenever the module fails to load. Hand it CSC through
      `TypedArray.set` — never the LP-text path. Pin the version: the low-level
      C API is on the `1.15.3-pre.*` line at time of writing and the stable
      line is one-shot text only.
- [ ] **A differential test across the two browser backends.** The same model
      solved by `highs-js` and by our simplex should agree on objective and on
      every dual, exactly as `differential.rs` already does natively against
      linked HiGHS. This is cheap to write and it is the only thing that will
      catch a marshalling bug that produces plausible-but-wrong prices.
- [ ] **Cancellation, designed before it is needed.** A worker blocked inside a
      solve cannot dequeue messages, so `postMessage({cancel})` arrives *after*
      the solve finishes and is useless. Three mechanisms, and we want the first
      and third: `worker.terminate()` as the guaranteed backstop from day one,
      and later a `SharedArrayBuffer` `Int32Array` flag polled through
      `js_sys::Atomics::load` every few milliseconds of iterations. The atomics
      flag works even from a non-atomics wasm build, because the SAB is a JS
      object rather than linear memory.

      **This constrains the solver, not just the UI.** `panic = "abort"` is set
      in the workspace profile, so a cancelled solve cannot unwind. Cancellation
      has to be a `Result` variant threaded through the simplex loop, and that
      is a decision to take before the loop is written rather than after.
- [ ] **Progress out of the solver.** A worker can `post_message` mid-computation
      without yielding, so a progress bar costs nothing structurally. The
      simplex needs to emit iteration count, current objective and the gap; the
      branch and bound needs nodes explored, incumbent and bound. Both already
      track these internally.
- [ ] **Memory budget, enforced rather than hoped for.** Budget **2 GiB**, pin
      `--max-memory` at link time, use `Vec::try_reserve` at the handful of
      sites that allocate the big matrices, and add a pre-solve size estimate
      that refuses politely instead of trapping the tab. Two facts make the
      last part non-optional: wasm memory **never shrinks**, so an instance
      permanently holds its peak, and an allocation failure under
      `panic = "abort"` is an unrecoverable trap that poisons the module. Treat
      the worker as disposable and re-instantiate rather than trying to recover.

      **Mobile is explicitly out of scope**, and this is a scoping decision
      rather than an oversight. Mobile Safari has been measured crashing pages
      at 100–200 MB with no catchable exception, which would cap the model size
      about an order of magnitude below anything worth modelling. A docked
      multi-panel studio with a solver in it is not a phone experience in the
      first place, so the constraint is being dropped rather than designed
      around: no touch input, no responsive small-screen layout, no mobile
      memory ceiling. Desktop browsers only.

      Desktop Safari stays in scope — it is nothing like iOS Safari for memory,
      and keeping it costs almost nothing, since `require-corp` works there and
      a self-contained studio has few third-party assets to make
      `credentialless` worth wanting.
- [ ] **Result transfer that does not copy twice.** wasm linear memory is not
      transferable, so "zero-copy to the UI" is not available. Copy the solution
      into a fresh `ArrayBuffer` in the worker and `postMessage` it with a
      transfer list: one memcpy, then an O(1) handoff. Return a few large flat
      `f64` arrays — primals, duals, nodal prices — plus a small JSON header.
      Never run `serde_json` over tens of megabytes. And never hand the UI a
      `Float64Array` view onto unshared linear memory: the next allocation
      detaches it and silently zeroes its length.

### Stage 1 — the thinnest thing that proves the whole pipe

One vertical slice, ugly on purpose, that exercises every boundary before any
design work goes in. If this works, nothing architectural is left to discover.

- [ ] Drag a MATPOWER or PSS/E file onto the page; it parses **in the worker**.
- [ ] Build and solve in the worker, progress bar ticking, UI still responsive.
- [ ] A results table: objective, per-bus prices, branch flows.
- [ ] Cancel mid-solve and get the tab back.
- [ ] The identical crate runs as a native desktop window, with HiGHS.
- [ ] Deployed to Vercel as static output and loading from a cold cache.

### The design system, which is a Stage 0 concern and not a Stage 6 polish pass

The reference implementation to study is Rerun's `re_ui`, because it is the only
professional-grade egui design system with public source, and it is the same
shape as what we need. It is **not** a dependency — it hard-codes Rerun's own
entity types and command set, and is published only so `rerun` can be — but as a
reference it is worth more than any amount of general advice.

- [ ] **Design tokens in a data file, not in the code.** `re_ui` keeps ~200
      semantic fields in RON files, Figma-derived, split into a global colour
      table plus per-theme alias maps. Ours can be smaller, but it must exist
      before the second screen is written, not after the twentieth.

      Concrete starting numbers, so this is not left as taste:

      **Contrast.** WCAG 2's 4.5:1 is *not enough on dark backgrounds* — the
      formula overstates dark-mode contrast, and `#767676` on white and
      `#949494` on black score the same 4.5-ish while being 27 APCA Lc apart.
      Target **≈10:1 for body text on the canvas**, which is where APCA's
      Lc 75 floor actually lands. VS Code's `#CCCCCC` on `#1F1F1F` is 10.26:1
      and is the best-validated anchor available. Conform to WCAG 2.2 AA
      formally; use Lc as the dark-mode sanity check.

      **Surfaces.** Invert the usual elevation model, as VS Code does: chrome
      *darker* than content, so the canvas is the brightest large surface. And
      never `#000000` for a large surface — it cannot express elevation.

      **Type.** Dense professional tools live at 11–14 px, not 16. VS Code's
      editor is 12 px on macOS, Figma's UI is 11 px, Blender is 11 pt, Rerun
      ships Inter Medium at 12 px with 16 px line height. Body 12, micro 11,
      header 13, title 16.

      **Spacing.** Base **4 px**, not 8 — the 8-point grid lacks the
      granularity for a dense tool. Radii 3 for widgets, 6 for panels.

      **Hit targets.** WCAG 2.2 SC 2.5.8 requires **24×24 px minimum**, and it
      is the hardest constraint on the row heights. A 20 px compact row with a
      16 px icon button violates it. The lawful fix is to make the *hit rect*
      24×24 while the painted glyph stays 16×16 — trivial in egui, since
      allocation and painting are separate.
- [ ] **Categorical palette: Paul Tol "light", not Okabe–Ito.** This is
      counter-intuitive and worth recording. Most colourblind-safe palettes were
      designed for white paper. Measured against a dark canvas, Okabe–Ito's
      black is **invisible** at 1.2:1 and its blue is marginal; Tol *muted* has
      three failures. Tol *light* — nine colours — clears 6.5:1 on every one.

      Cap categorical series at **8**; beyond that use small multiples or
      hover-to-highlight. Encode every series as **hue plus dash plus marker**,
      never hue alone, and use this as the acceptance gate: **desaturate the
      chart; if the series are still separable, it ships.** For heatmaps use a
      lightness-monotonic ramp such as viridis, since lightness survives every
      form of colour vision deficiency — explicitly *not* the blue-to-red ramp
      the incumbent power tools use, which is the CVD-hostile case.
- [ ] **Enforce the tokens with a lint, or they rot.** This is the single most
      copyable idea in that codebase: `clippy.toml` *bans*
      `Color32::from_rgb`, `from_gray`, `hex_color` and every `Rgba`
      constructor with the message "Do not hard-code colors — declare them in
      design_tokens". They also ban `egui::Ui::checkbox` and `Spinner` in favour
      of their own. A design system with no enforcement mechanism becomes
      decoration within a month.
- [ ] **Light theme from day one.** Rerun added theirs roughly two and a half
      years after the token system, described the work as "a plethora of ugly
      hacks", and shipped it marked experimental. Retrofitting a second theme
      through a codebase full of `match theme` is the expensive path, and it is
      entirely avoidable by having two token files from the first commit.
- [ ] **One composable list primitive rather than twenty bespoke widgets.**
      Nearly all of Rerun's panel UI is a single `ListItem` plus a
      `ListItemContent` trait, with implementations for label, two-column
      property, and button. Inspectors, trees, scenario browsers and result
      lists are all the same primitive. Build that first; everything else gets
      cheaper.
- [ ] **Two immediate-mode mechanisms egui does not give you and a dense UI
      needs.** A *frame-lagged layout accumulator*, so a property column can be
      aligned across a whole nested tree — contents register their desired width
      in frame *n* and it is applied in frame *n+1*. And a *full-span* mechanism
      that walks the `UiStack` to find the enclosing panel, so a row highlight
      can span the full width inside a scroll area inside a margined panel.
      Both are load-bearing for anything that looks professional, and both are
      non-obvious enough to be worth knowing before hitting them.
- [ ] **Token hot-reload in developer builds only**, behind a `build.rs` cfg
      gate so it costs nothing in release or wasm. Theme iteration goes from a
      recompile to instant, and that difference decides whether the design
      actually gets tuned.
- [ ] **A runnable example app for the design system**, the way `re_ui` ships
      one. It is the component catalogue, the manual test surface and the
      screenshot source, and it stops the system drifting from the app.
- [ ] **Write the copywriting rules down.** Rerun's `DESIGN.md` is short and
      right: no full stop on single-sentence UI text but always on multi-sentence
      text; sentence casing, never Title Case except product names; `…` on
      buttons that need further input; **no colon after labels**; spaced em dash
      for parentheticals. Cheap to adopt now, invisible-but-pervasive later.

### Stage 2 — the studio shell

- [ ] **`egui_tiles` 0.16** for docking: a viewport, an inspector, a run/console
      panel and a scenario browser, rearrangeable and persisted between
      sessions.

      Chosen over `egui_dock` 0.20 deliberately. `egui_dock` has two things
      tiles lacks — tear-off floating panels and built-in per-tab scroll areas
      — but the first matters much less in a browser, where its "floating
      windows" are egui windows inside the same viewport rather than real OS
      windows, and the second is a few lines to add yourself. Against that,
      `egui_tiles` has **n-ary containers including a grid layout** where
      `egui_dock` has binary splits only, and it is owned and funded by
      rerun-io with several regular contributors, where `egui_dock` is a
      one-person project whose author has already renamed their GitHub account
      once, silently invalidating every hard-coded URL. Rerun ships `egui_tiles`
      and does not depend on `egui_dock` at all.

      Neither has undo of layout changes. `egui_tiles` at least exposes
      `Behavior::on_edit(EditAction)` as the hook to build one on.
- [ ] A command palette. It is the cheapest discoverability mechanism there is
      and it makes every later feature findable without menu archaeology.
- [ ] **Undo/redo, scenarios and provenance are one mechanism, not three.**
      This is the structural insight worth designing around, and it arrived from
      two directions at once.

      From the domain side: the best scenario management in the incumbent field
      is Antares Web's variant manager, where **a variant is not a copy of a
      study — it is the base plus an ordered list of commands**, each with an
      action and arguments, addressable over an API. That single decision buys
      real diffs between scenarios, replay onto a new base dataset, storage
      deduplication, and an audit trail.

      From the tooling side: Rerun gets undo and redo **for free** because their
      UI state is itself a recording, so undo is time-travel over a log they
      already had, not a separate stack.

      Same mechanism. So: model edits are named commands appended to a log; undo
      is a cursor into it; a scenario is a base plus a command sequence; and
      provenance is the log itself. Retrofitting any one of these later is a
      rewrite. Designing the log first makes all three nearly free.

      It also answers the API-versus-GUI question below, because every GUI
      action already *is* a named, printable, scriptable command.
- [ ] Project save/load, and autosave into OPFS. OPFS is Baseline-wide since
      2023; the File System Access API is Chromium-only in 2026 and must be a
      progressive enhancement, never the load-bearing path.

### On stage order: results before the editor

Stages 3 and 4 below are written editor-first, and there is a good argument for
inverting them that is worth recording even though the numbering stays.

A results view — table, convergence plot, honest `Status` rendering — proves
the engine to a user and needs **no editing model, no coordinates, and no undo
stack**. It also forces the streaming-progress and cancellation path, which is
the riskiest boundary in the whole design, to be built against the simplest
possible UI rather than against a half-finished canvas.

The editor is the more impressive demo. The results view is the better first
milestone.

### One thing we have to invent, because nobody else has needed it

**A visual state for "stale".** Across every tool surveyed — Blender, Houdini,
Grasshopper, Unreal, Nuke, TouchDesigner — recomputation is fast enough that
"edited but not yet re-solved" essentially never renders. Houdini has a
*needs to cook* badge and hides it by default. Node-RED's blue dot for
undeployed changes is the closest prior art anywhere.

At our solve times — seconds to minutes — **stale is the dominant state**, and
it should be the loudest non-error signal on the canvas. It is also the honest
one: a result that no longer matches the model is the single most dangerous
thing a modelling tool can display, and it is exactly what every incumbent
does when a run is left open beside an edited network.

Corollary for the architecture: **every `Solution` carries the graph version
that produced it**, and any component whose version differs renders stale
rather than wrong. Editing during a solve is always allowed and simply
supersedes the run — blocking edits during compute is the precise failure the
worker architecture exists to prevent.

### Stage 3 — the network editor, which is the actual product

- [ ] Canvas with pan/zoom, marquee select, snapping, and a minimap. Rendered
      through `wgpu` in an egui panel so it stays smooth at thousands of nodes.

      **`egui-snarl` 0.11 is the widget to start from**, and the reasoning is
      worth keeping because the obvious alternative is a trap. It is the only
      candidate simultaneously on egui 0.35, with real pins, headers, box
      select and serde, offering **both** bezier and orthogonal wire styles,
      and doing pan/zoom the modern way through `egui::Scene` and a layer
      transform rather than by mutating stored node positions.

      **Do not use `egui_node_graph` or `egui_node_graph2`.** The original is
      gone — the author deleted every repository and **all crates.io versions
      are yanked**; the fork is pinned to egui 0.29 and last touched in 2024.
      That lineage's living continuation is `egui_node_editor` on GitLab, but
      its zoom mutates node positions, which is the correctness smell snarl
      deliberately moved away from.

      What snarl does not have, and we therefore own: **no undo, no comment
      frames or node groups, no minimap.** Undo is already Stage 2's problem
      and belongs over model edits rather than in the widget, so that is
      alignment rather than a gap. Groups and a minimap are ours to add.

      **`egui-snarl` culls wires but not nodes.** Its draw loop iterates the
      entire draw order and builds a full `Ui`, with per-pin sub-`Ui`s, for every
      node regardless of whether it is on screen. At thousands of nodes that is
      layout-bound before it is paint-bound. Culling node *construction* is
      therefore ours to add — see the performance traps below, where this is the
      general rule rather than a snarl quirk.

      Worth watching rather than adopting: **`egui_graph`** from nannou-org,
      very active through 2026, also `Scene`-based, and it already has waypoint
      edge routing, snap-to-grid with alignment guides, and a socket-aware
      layered auto-layout — all things we will eventually want. It trails one
      egui version. If it reaches 0.35 before Stage 3 starts, re-evaluate.
      `egui_graphs` is a different tool: graph *visualisation* with pluggable
      force-directed and hierarchical layouts, no pins or wire-dragging. It may
      still earn a place for auto-layout of large view-only networks.
- [ ] Node and edge editing: buses, lines, generators, loads, storage. Typed
      inspectors generated from the model types where possible.
- [ ] **Live rebuild on edit.** This is the thesis the whole engine rests on and
      it is still, per gap 4 in the README, untested as a workflow. The
      measurements say it holds: an edit at regional scale rebuilds in single
      -digit milliseconds in the browser. Prove it with a real edit loop.
- [ ] Geographic layout when coordinates exist, force-directed when they do not.
- [ ] Large-network behaviour: level-of-detail, culling, and a decision about
      what a 13,659-bus network even looks like on screen.

### Canvas interaction patterns worth stealing, from tools that got them right

Surveyed from Grasshopper, TouchDesigner, Substance 3D Designer, Houdini, Nuke,
n8n and ComfyUI. Listed because these are the details that separate a canvas
that feels like an instrument from one that feels like a demo, and because most
of them are cheap if designed in and expensive if retrofitted.

- [ ] **Encode data *shape* in the wire itself.** Grasshopper draws a single
      grey line for one item, a **double line for a list**, a **dashed double
      line for a tree**, and turns the wire **orange when no data is flowing**.
      A user reads the structure of their model without opening anything.

      This maps onto our domain better than it does onto Grasshopper's: a link
      here is a scalar, a time series, or a scenario-indexed series, and getting
      that wrong is one of the most common modelling errors there is. A wire
      that shows its own dimensionality would catch it at a glance.
- [ ] **Per-wire display modes: default, faint, hidden.** Also Grasshopper. A
      hidden wire still carries data; selecting either end draws a ghost wire so
      the connection is recoverable. This is how a dense graph stays readable
      without deleting information, and it is a per-input property rather than a
      global toggle.
- [ ] **Type-aware connection.** Substance filters its node-creation search by
      connector type when you drag off a pin, and offers a link mode that
      *prohibits* connections between mismatched usages. Refusing an invalid
      wire at connect time is worth more than reporting it at solve time.
- [ ] **Highlight flow.** Substance can highlight everything upstream or
      downstream of the selection. On an energy network that is "what feeds this
      bus" and "what does this unit affect", which is a question users ask
      constantly and currently answer by tracing wires with a finger.
- [ ] **Keep label text at constant screen size past a zoom threshold.**
      Substance does exactly this, and it pairs with the egui font-quantisation
      trap recorded below: if labels stop scaling past a threshold, the glyph
      cache stops missing. The performance fix and the legibility fix are the
      same fix.
- [ ] **Box-select direction should carry meaning.** Grasshopper inherits
      Rhino's convention: left-to-right selects only fully enclosed nodes and
      draws a solid rectangle; right-to-left selects anything touched and draws
      a dashed one. Free to implement, and instantly familiar to anyone from a
      CAD background — which is our audience.
- [ ] **Selection and wire modifiers, taken wholesale** because they are
      near-universal: shift-drag adds to a selection, ctrl-drag subtracts,
      ctrl-shift-click toggles. On a pin, shift-drag adds a wire without
      replacing the existing one, ctrl-drag erases, ctrl-shift-drag moves every
      wire at once.
- [ ] **Type-to-create in the canvas search.** Grasshopper turns `123` into a
      slider preset to that value, `1<5` into a slider with that domain,
      `//text` into a panel and `~text` into an annotation. The equivalent here
      writes itself: a number becomes a parameter, `bus7>bus9` becomes a line,
      a carrier name becomes a generator. This is the single largest speed
      difference between an expert and a novice in these tools.
- [ ] **Show computation on the wires.** TouchDesigner animates dashed wires
      along the path that is currently cooking. For us that is a live picture of
      which part of the model is rebuilding, and it costs almost nothing.
- [ ] **Frames, comments and pins as first-class document objects**, not
      decorations: a frame that moves its contents and auto-fits, comments that
      can be parented to a node and die with it, and navigation pins that a
      hotkey cycles through. All three are what make a large graph navigable
      by someone who did not build it.

**On minimaps, where the evidence is genuinely split.** Nuke's design is the
best of those surveyed: the minimap **appears automatically only when the graph
exceeds the viewport and disappears when it fits**, which removes the clutter
objection entirely. ComfyUI's is the second good idea — it renders **error and
bypass state**, so it is a status overview rather than a locator. Blender
deliberately declined to build one, a core developer preferring "semantic zoom"
— showing nodes differently when zoomed out — and that objection is worth
taking seriously rather than dismissing. Houdini, TouchDesigner and Nuke all
bind theirs to a single key. Build it late, make it status-bearing, and let it
auto-hide.

- [ ] **Auto-layout: scope it, stabilise it, and make it undoable.** The
      evidence here is unusually clear. "Tidy selection" is welcomed; "tidy
      everything" is resented — n8n users report their whole-graph tidy
      "makes large workflows look worse, not better. Everything gets stretched
      into one long vertical line." Their most-requested unmet feature is simply
      **spacing controls**, which were never shipped.

      The stability problem has a name and a citation: Misue, Eades, Lai and
      Sugiyama, *Layout Adjustment and the Mental Map* (1995) — the same
      Sugiyama who invented layered layout in 1981 also wrote the paper warning
      that re-laying-out from scratch destroys the user's understanding. A user
      complaint from 2022 puts it more plainly: "a small change to the structure
      will typically cause a full re-layout, and place nodes completely
      differently to the previous output, making visual/mental comparisons
      between versions quite taxing."

      The cheap mitigation, verified in n8n's source, is to **feed the layout
      engine nodes and edges pre-sorted by their current position**, so that
      equal-cost orderings resolve to the arrangement the user already has.

      For the implementation, **`rust-sugiyama`** is the right starting point:
      a full five-stage pipeline — cycle removal via petgraph's greedy feedback
      arc set, network-simplex ranking, weighted-median crossing reduction,
      Brandes–Köpf coordinates — built directly on `petgraph`, MIT licensed, and
      citing exactly the same lineage as dagre and ELK's defaults. Avoid
      `graphviz-rust`, which shells out to a native binary, and `elkjs`, which
      is transpiled Java.

### egui performance traps, found before hitting them

Four of these are specific to a zoomable canvas with text on it, which is
exactly what Stage 3 is. Worth reading before writing the canvas rather than
after profiling it.

- [ ] **Quantise font sizes on the canvas. Do not scale text continuously with
      zoom.** egui replaced its glyph rasteriser in 0.34 — `ab_glyph` out,
      `skrifa` and `vello_cpu` in — and the glyph cache key hashes the exact
      float bits of the scale factor. A continuously varying text scale
      therefore misses the cache **every frame** and re-rasterises from
      outlines, and the new rasteriser is 3.6× to 6.5× slower per glyph than
      the old one depending on size. The tracking issue is open, upstream has
      said dynamically scaling text is not a project priority, and the reporter's
      own fix was to render all text at one size. So: snap label sizes to a
      small ladder of discrete steps as the user zooms. This is the single
      biggest performance decision in the canvas and it is invisible until it
      is too late to change cheaply.
- [ ] **Cull at widget construction, not at paint.** Tessellation is **not**
      cached between frames — egui rebuilds and re-tessellates everything, every
      frame, deliberately (comparing shapes costs about half of what
      tessellating them does). But measured on a realistic frame, tessellation
      is only ~27% of the cost; layout and widget logic dominate. So skipping
      `Shape`s is not the win — skipping the `ui.add()` call entirely for
      off-screen nodes is. Compute the visible world rect first and never
      construct off-screen node UIs at all.
- [ ] **Draw wires as one mesh, not N shapes.** `Painter::add` returns an index
      that can be back-patched, so a `Shape::Noop` placeholder can be reserved
      before nodes are drawn and filled with a single assembled mesh afterwards.
      That is how wires get painted behind nodes in one primitive rather than
      thousands.
- [ ] **Set `subpixel_binning = false` and keep `font_hinting = true`.** Both
      are new knobs. Binning renders each glyph at up to four fractional offsets
      for smoother kerning and egui's own docs concede it "lead to text looking
      more blurry". For a dense technical UI, crisp beats evenly-kerned.
- [ ] **Ship a real UI font.** egui has no system-font loading, and the bundled
      proportional default is Ubuntu-Light — a poor fit for dense professional
      interfaces. Pick one deliberately and register it in `FontDefinitions`.
- [ ] **Use MiMalloc in the native build.** One reported case went from 120 ms
      per frame to a stable 60 fps purely by swapping the allocator, with GPU
      time at 0.8 ms and ~90% of the cost in egui's own layout. egui's own
      benchmarks pull in MiMalloc for the same reason.
- [ ] **Pin the egui version and expect quarterly breaks.** Releases are roughly
      quarterly with breaking changes each time, and 0.35 removed everything
      previously deprecated, so 0.34 → 0.35 is a hard break for anyone who
      ignored the warnings. Note also that `egui_plot` is on its **own** version
      line — 0.36 pairs with egui 0.35 — so version parity cannot be assumed
      across the ecosystem.

### What has actually shipped in the canvas so far

Recorded here rather than only as ticks in the stages, because the stages were
written before any of it existed and the decisions below were made against the
code rather than in the plan.

- Buses as busbars, not dots. Circuits tap onto them perpendicularly at points
  spread along the bar and ordered by which way the far end lies, so nothing
  crosses on the approach. A bar has length because more than one thing
  connects to it; a diagram where everything meets at the midpoint throws that
  away.
- Generators, loads and storage as the IEC symbols — a ring with a sine, a
  solid arrowhead, battery plates — degrading to a filled disc below five
  pixels of radius rather than drawing three pixels of squiggle inside a ring.
- Injection above the bar, withdrawal below, which is the convention.
- Selection as viewfinder brackets and hover as a thin closed outline. They
  were both rings a pixel apart in size, which is one ring as far as a reader
  is concerned.
- An inspector: nodal price first, then what is attached, then dispatch against
  nameplate, then unserved energy where there is any.
- Keyboard camera (F fit, escape clear, plus/minus zoom), ignored while any
  widget holds focus. Commanded moves ease; dragging and scrolling do not,
  because a camera lagging a pointer reads as lag rather than as motion.
- An empty state that names the action and offers the sample case.

Still missing, in rough order of how much they are missed: a legend for the
corridor colours (AC, transport, link are three hues nothing explains), any
plot at all, a font that is not Ubuntu-Light, and multi-snapshot navigation —
every reduction on the canvas is currently over the whole horizon because
there is no way to ask for one hour of it.

### Stage 4 — results, and being honest about them

- [x] Flows on the network, brightening with loading, with a tick across any
      corridor sitting on its rating. Not animated by direction: the DC model
      returns a signed flow per snapshot, and marching ants down a corridor
      would encode a sign we already have room to state, at the cost of a
      permanently moving canvas. Hovering a circuit gives flow against rating.
- [x] Nodal prices on the network itself — the output the engine exists to
      produce and the one competing browser tools cannot show, because the
      pure-Rust LP alternatives do not return duals.

      **Not a heatmap.** Lightness on the busbar, scaled to the network's own
      spread. Hue in this interface means state, and a fourth colour competes
      with the ones that carry it; lightness is also the channel that survives
      colour vision deficiency, which the blue-to-red ramp everybody reaches
      for does not. An absolute scale would render an uncongested network as one
      flat colour, and where prices stop being equal is the whole point.
- [ ] Dispatch stacks, price duration curves, storage state of charge,
      capacity build-out by period. `egui_plot` for all of it.
- [ ] **Show the *status* of an answer, not just the number.** An AC result that
      is a relaxation rather than an operating point, a head iteration that did
      not converge, and a branch and bound that stopped on its node limit are
      all things the engine reports and an interface would be wrong to hide. A
      result with an `OPEN` gap must look different from a proved optimum.
- [ ] **Infeasibility diagnosis, built on priced slack rather than on IIS.**
      The largest, best-defined, least-served problem in this field.
      Experienced users report spending 15–30 minutes per infeasible model, and
      their three strategies are: inspect an IIS, toggle constraints off, add
      slacks.

      **Do not build this on IIS.** It is the obvious choice and it is the wrong
      one. Computing one is "in general not easier than solving the original
      model"; the result is non-unique, often large, and sometimes flags every
      constraint; in PyPSA the feature silently requires Gurobi; and HiGHS's
      implementation is LP-only and self-described as not yet robust. One
      published worked example had the IIS flag all five constraints and
      "not clearly point to the actual issue".

      **Build it on elastic slack, which is cheap, solver-agnostic and always
      available.** Auto-insert priced slacks on demand balance, reserve, CO₂
      budget, capacity limits and cyclic state-of-charge; solve the elastic
      problem; report **violation magnitude per bus per timestep**, ranked, and
      render it straight onto the network and the timeline. That answers "how
      infeasible, and where", which is the question. PyPSA teaches this by hand
      with a load-shedding generator; PLEXOS institutionalises it as unserved
      energy priced at value of lost load. We should make it a button.

      Then two more tiers: **fix-to-a-candidate-and-report-violations**, the
      cheapest useful diagnostic there is; and IIS as an opt-in second opinion
      where the solver supports it.

      The part that is genuinely ours either way is translation. "Rows 40,117
      and 40,118 conflict" is not an answer. "Unit 7's 14:00 ramp limit cannot
      meet load after you cut Aachen–Liège" is. Nobody currently renders an
      infeasibility as a highlighted subgraph of the network with the offending
      timesteps, and that is a real gap rather than a crowded space.
- [ ] **Treat "solved but wrong" as a first-class failure state.** A real
      published workflow wrote a results file for a run that returned
      `Status: ok`, `Termination: suboptimal`, `Objective: 3.14e+37`. Another
      produced a completed run with all-NaN demand after a year mismatch made
      every European load null.

      So: never emit a result artefact for a suboptimal status, an absurd
      objective magnitude, a hit MIP gap or a hit time limit without a loud,
      structural marker attached to the run **and inherited by every chart
      derived from it**. A wrong answer with a beautiful stacked bar chart on
      top of it is the worst thing this tool could ship.

### Stage 5 — scenarios, which is what makes it a studio

- [ ] A scenario is a base plus a command sequence, per Stage 2. Never a copy of
      a file. The failure mode being avoided is documented and universal: when a
      scenario is a copy, teams end up with "completely separate projects", and
      then nobody can say what differs between run 7 and run 34.
- [ ] **Runs as immutable, content-addressed artefacts** carrying their full
      input configuration, solver log, status, objective, timings and code
      version. A real published workflow re-ran with changed settings and left
      the archived config reflecting the *first* run; that class of bug should
      be impossible by construction rather than caught by discipline.
- [ ] Cross-run comparison as a query, not as "load ten result files". The scale
      to design for is **100–1,000 runs** — that is the empirical range in the
      parameter-space literature, and one real utility resource plan used 67
      scenarios across 100 simulations. Handle the genuinely hard case too:
      comparing runs at *different spatial or temporal resolution*.
- [ ] **Generate the sample set in the tool.** Of twenty-one surveyed
      parameter-space tools, only four let users create the samples inside the
      tool; everyone else scripts it by hand. Full-factorial, one-at-a-time
      sensitivity, Latin hypercube, and near-optimal exploration, with a queue
      view and parallel dispatch.
- [ ] The comparison view: what changed, what it cost, which constraints bound.
- [ ] Emit IAMC-format long tables. It is the lingua franca of model comparison
      exercises, and a tool that cannot produce it cannot participate in one.

### What modellers actually do, and the two holes nobody has filled

Researched against primary sources — tool documentation, issue trackers, and
the time-series-aggregation review literature. Everything below is either
quoted from a source or flagged as inference.

**Two findings change what to build first.**

#### 1. Licence-free infeasibility diagnosis is available and nobody has it

PyPSA's own troubleshooting guide offers four strategies for an infeasible
model, and the first — `n.optimize(compute_infeasibilities=True)` — is gated on
**"If you are using Gurobi"**. Its flagship *Tracing Infeasibilities* example
runs `n.optimize(solver_name="gurobi")`. The leading open-source energy
framework's flagship infeasibility tutorial requires a commercial licence.

Calliope's equivalent page is the honest one and it is grim: its opening advice
is to *remove constraints*, which is bisection by hand, and the full toolkit
runs to twelve techniques including writing an LP file out to run
`gurobi_cl ResultFile=...` against it, digging through `.cplex.log`, and calling
`model.backend.verbose_strings()` first **because by default the LP file has
unreadable names**.

Meanwhile **HiGHS has `Highs::getIis`**, with a strategy bitmap that includes
forming an infeasibility set by solving an elasticity LP and then reducing it
toward a true IIS — and HiGHS compiles to WebAssembly, which this repo has
already measured. Two caveats worth carrying: the facility was still receiving
bug fixes as of v1.15.1, and its docs suggest it is LP-only, so a model with
integer investment decisions likely needs the relaxation IIS'd instead.

**The differentiator is not the IIS.** It is translating row indices back into
"generator X at hour Y conflicts with binding constraint Z". Calliope shipping
`verbose_strings()` as an opt-in is direct evidence that the incumbents treat
that mapping as an afterthought, and it is the part this repo is best placed to
own — the model builder here knows what every row is.

The elastic-slack path is already half-built as priced load shedding, and it is
what the community hand-rolls: Calliope ships `unmet_demand`, PyPSA's guide
recommends adding a high-cost generator at every bus. It is also error-prone
when hand-rolled — PyPSA-Eur issue #1907 reports load shedding reported **1000x
larger than expected** because of a sign-attribute scaling mistake. A 1000x
error in the diagnostic used to diagnose everything else.

#### 2. Network diff is an open request from a PyPSA maintainer

Issue #1627, "Make comparison of `networks` more accessible", opened by a core
contributor. Three motivations: verifying that a scenario modification was
actually applied, debugging networks that "suddenly become inexplicably
infeasible", and the inadequacy of the current workaround. The stated blocker is
that **pandas `compare()` requires the same labels for index and column, which
is rarely the case**.

What PyPSA has today is equality and not difference: `==` since v0.29.0,
`n.equals()` since v0.35.0. Both return booleans.

A network diff is not a dataframe diff, which is why pandas cannot do it. It is
three different things needing three presentations:

- **Topological** — components added, removed, re-parented. Set difference on
  identity, which is exactly where pandas dies on misaligned indices.
- **Parametric** — scalars changed on surviving components. The only part
  pandas handles.
- **Time series** — an 8760-vector changed. Never 8760 numbers; a duration-curve
  overlay and summary statistics. Antares independently confirms this is its own
  category by giving it a separate `replace_matrix` command and storing matrices
  out of band from the command log.
- **Result delta conditioned on input delta** — "you changed three inputs, here
  are the seven outputs that moved most". Nothing open does this.

It must work between an unsolved and a solved network, which is explicitly
requested — meaning inputs and results have to be one addressable object rather
than results being a separate artefact.

#### The scenario-as-commands design is confirmed, and improvable

Stage 2 records Antares Web's variant manager as base-plus-ordered-commands.
That is verified — "Any modifications made to a variant are recorded as a list
of commands in the variant's history" — and its command vocabulary is worth
copying nearly directly, because its *shape* carries the insight: structural
CRUD (`create_area`, `remove_link`, `create_st_storage`, ...) plus
`replace_matrix` for time series plus `update_config` for scalars. **Three kinds
of delta, not one.**

Two things to take from elsewhere:

- **Spine Toolbox composes declaratively**, not imperatively: scenarios are
  built from *ranked layers of alternatives*, where position determines rank.
  That gives reuse an ordered command list does not — `base + {high_gas_price} +
  {no_nuclear}`, with those layers shared across scenarios.
- **PyPSA-Eur shows the failure mode.** It encodes scenario identity into the
  *filename* (`base_s_{clusters}_elec_{opts}.nc`). A filename is a lossy hash of
  a config, so when it collides or when `config.yaml` changes underneath, the
  provenance is silently gone.

Antares also could not make its own mechanism universal — variant management
applies "only to 'managed' studies available in the 'default' workspace".

So: **named, content-addressed, ordered sets of reusable typed deltas over an
immutable base, with matrices stored by hash out of band.** Antares'
vocabulary, Spine's composition, neither one's provenance hole.

#### Where the time actually goes is not data cleaning

The prior assumption was data wrangling. The evidence points somewhere more
specific and more favourable. A 2024 REMix scenario-ensemble paper reports
**700 scenarios, 3,400,000 files in 260,000 directories, 33 TB** — and that it
took **three years for a team of ten** to stabilise the workflow, against 20
days of actual solving. They had to invent a hierarchical directory structure
just to track file dependencies.

Pfenninger et al. (2017) name the same thing: it is time-consuming to "track
data provenance and processing steps". A 2024 study on model understandability
finds PhD students "invest large amounts of time to get up and running" and
cannot determine "where the most relevant uncertainties in the model lie".

**The dominant sink is the combinatorial bookkeeping of many runs and the loss
of the chain from a result back to the assumption that produced it.** That is a
provenance and diffing problem, which suits a studio far better than a
data-cleaning problem would.

#### What to draw, from revealed preference

PyPSA's `statistics` module and PyPSA-Eur's automatic per-run outputs encode
years of "users kept asking for this". Both are dominated by the same handful:

- **The energy balance is the atom.** It appears in the metric list, the summary
  CSVs, a map view, a time-series view and an interactive view. One primitive —
  per bus, per carrier — renders as a stack, a map and a heatmap.
- **Duals are a headline statistic, not an advanced one.** "Marginal prices" is
  on the canonical list and gets its own heatmap. Already done here, and worth
  noting that retaining duals gives constraint shadow prices nearly free, which
  is what PyPSA #1732 wants soft constraints for.
- **The hour-of-day by day-of-year heatmap is the workhorse chart**, applied to
  prices, utilisation and storage state of charge alike. One component, three
  domains.
- **Curtailment and market value are top-level**, not derived afterthoughts.
- **Capacity and generation must look different.** PyPSA keeps
  `Installed`/`Expanded`/`Optimal` capacities as three separate metrics and
  `Supply`/`Withdrawal` separate from all of them, because conflating "GW built"
  with "GWh produced" is the classic reader error.

#### Temporal aggregation, which has a literature and a stated best practice

From Teichgraeber & Brandt's review. The domain spans **eight to nine orders of
magnitude in time scale**. Aggregation buys one to two orders of magnitude of
compute, non-linearly: **10 representative days out of 365 (2.7% of the data)
reduces computation time to 0.1-1.1%** of the original.

Their best practice #1 is that performance **"should be validated on the full
time series"** — and nothing found shows a user whether their aggregation is any
good. That is a cheap, high-credibility thing to build: a duration-curve overlay
of original against aggregated, per series, with the surviving extreme days
marked.

The correctness trap is storage. Aggregation assumes periods are operationally
independent, which fails for seasonal storage, where **"the order in which the
cluster days appear matters"**. The fix is intra-period plus inter-period state
of charge superposition with the daily cyclic constraint replaced by a yearly
one. This should be a visible toggle rather than a buried flag.

#### The competitive picture

`model.energy` is the closest browser-native thing and is deliberately a toy —
its own words are "a toy model with a strongly simplified setup", with no
inter-regional transmission, four selectable weather years, and **server-side
solving**. Antares Web is the most serious web application in the space and is a
*study manager*: the solve is not in the browser.

TransitionZero's FEO — an OSeMOSYS-based platform covering 163 countries that
added a web model builder in 2024 — **no longer resolves at
`feo.transitionzero.org`**, while the parent domain does. Worth finding out what
happened before assuming the space is empty for a good reason.

#### What to deprioritise

**N-1 contingency screening.** It belongs to a different profession than the one
this tool serves — TSO operations rather than system planning — it needs AC power
flow to be credible, and it is served by entrenched incumbents with regulatory
sign-off. MISO screens more than 11,500 contingencies every four minutes; that
is not a thing to compete with from a browser tab.

### What the domain research says to build, and what to refuse

Ordered by how much each converts the tool from demo to instrument. Most of
this is cheap; almost none of it is glamorous.

- [ ] **Validate inputs at the boundary, before the solver sees them. This is
      the highest-leverage single feature in the entire plan.** The only
      published usability study of open energy frameworks found the two
      most-mentioned problems were *input data handling* and *error messages*.
      The canonical horror: a missing value in an input CSV surfaced to a user
      as an out-of-memory crash inside the solver layer. Another: a year
      mismatch made all demand NaN and the run "completed".

      Ship a named, catalogued check suite — min greater than max, ramp-at-start
      below ramp limit, existing capacity above the cap, NaN or negative
      availability, name mismatches across tables, cyclic state-of-charge
      conflicting with a specified initial state, islanded buses carrying load,
      reserve zones with no firm resource, timezone and DST misalignment, unit
      inconsistency. **None of these needs a solver**, and each one converts a
      baffling failure into a sentence.
- [ ] **Errors must surface at the layer that caused them**, naming the file,
      row, column and value. Owning the error text end to end is most of the
      perceived quality of a technical tool.
- [ ] **Code and GUI as peers.** Of users certain they would adopt a modelling
      platform, **71% want a programming language as the interaction basis**,
      18% a web app, 6% a desktop GUI — while policymakers largely will not
      drive a model directly at all. A GUI-only tool loses the modellers; an
      API-only tool loses everyone else. The command log resolves this: every
      GUI action emits a named command the user can read, copy, script and diff.
- [ ] **Exploit the fast build for a genuinely interactive loop — this is the
      strategic differentiator and nothing else has it.** Build time is a
      first-class cost that solver benchmarks hide: one framework takes ten
      minutes to build a model that solves in two seconds. Our 96 ms build only
      matters if the interface spends it on perturb-and-resolve rather than on
      batch runs. Ship a **draft mode** — reduced timesteps, relaxed
      integrality, representative days — that runs in-page with a visible
      fidelity contract and one-click promotion to a full run.
- [ ] **Shadow prices as a first-class, explained output.** The flagship open
      European model's default plotting pipeline has **no shadow-price map at
      all**. Duals are how a planner learns *why* the optimum is what it is.
      Nodal prices on the network, binding-constraint frequency over the year,
      and the sign convention explained on screen, because misreading it is the
      most common error.

**And what to refuse, each for a measured reason:**

- [ ] **No single-line diagrams.** Zero occurrences across four major published
      planning and market documents. It is an operations artefact for a
      different persona, and it is months of work aimed at the wrong user.
- [ ] **No contour maps by default.** A study with thirty professional
      power-system engineers found participants performed *worse* at excursion
      identification with contours than with glyphs, and were less confident.
      Contouring also alters the statistical dispersion of bus values and
      destroys the extremes you were looking for. Opt-in, with the caveat shown.
- [ ] **The default figure set, and only it**, drawn from a census of what real
      published documents actually contain: capacity and generation mix as
      stacked bars by year and scenario; dispatch stack for a chosen window;
      average hourly profile; geographic network map with flow and capacity
      glyphs; **sensitivity and tornado charts** — 40% of one real resource
      plan's figures, and nobody builds them well; load and price duration
      curves; and an hour-of-day by day-of-year heatmap. Every figure exports to
      vector **and** to the underlying tidy data, because "reformatting outputs
      into charts for non-modelling audiences" is a named time sink.
- [ ] **Do not break the study format.** Every framework surveyed has done it
      and every one paid in lost reproducibility — one could not load its own
      previous version's models at all. Content-hash the inputs, version the
      schema from the first commit, and write the migrator before it is needed.

### Testing the interface

Untested UI code rots faster than anything else, and a canvas app resists the
usual tools: there is no DOM, so Playwright selectors have nothing to grip.

Set expectations honestly first. **Nobody in this ecosystem has solved automated
browser end-to-end testing for canvas GUIs.** Rerun, the largest egui app in
existence, has no Playwright at all; its confidence comes from `egui_kittest`
image comparison run *natively* plus a manual pre-release checklist. egui itself
ships PR previews for humans to click. Plan around that rather than against it.

- [ ] **`egui_kittest` 0.35 for interaction tests, and make this the bulk of the
      suite.** It queries an AccessKit tree the way Testing Library queries a
      DOM — `get_by_label`, `get_by_role_and_label`, then `.click()`,
      `.type_text()`, `.focus()`. No GPU needed for that path, so it is fast and
      deterministic.
- [ ] **Run the identical `egui_kittest` suite against wasm in headless Chrome.**
      This is the most valuable and least documented fact found in the research,
      and it was verified by execution rather than read in a doc:
      `egui_kittest` with default features **compiles to
      `wasm32-unknown-unknown` and its tests genuinely run in a real browser**
      under `wasm-bindgen-test`. So the same interaction tests cover both
      targets, and the wasm run catches the class of breakage a `cargo check`
      misses — a native-only crate sneaking in, `std::time::Instant`, a thread.
- [ ] **Snapshot tests, native only.** Also verified by execution:
      **image snapshots do not work in the browser.** `egui_kittest` removes
      `Backends::BROWSER_WEBGPU` because its readback path uses blocking
      `pollster::block_on`, so adapter creation fails in-page. Run snapshots on
      one platform — lavapipe software rasteriser on Linux, which is
      deterministic across runners — store the PNGs in git LFS, call
      `remove_cursor()` and `fit_contents()` before each, and batch with
      `SnapshotResults` so one run reports every failure rather than the first.
      Keep the image count small; prefer plain assertions to pixels.
- [ ] **Accessibility assertions, and note where they do and do not reach.**
      `ctx.enable_accesskit()` makes role, label, disabled, hidden, toggle state
      and numeric value directly assertable, and tab-order tests are the
      cheapest real accessibility guarantee available — press `Tab`, assert
      focus lands where it should, including wraparound. **But AccessKit has no
      web adapter**; it is listed as planned. So this is a *native-build*
      guarantee that we rely on to keep the web build honest by construction,
      not something assertable in-page. Worth knowing before someone promises
      screen-reader support on the web.
- [ ] **Frame convergence as the performance gate, not wall clock.**
      `Harness::run()` returns the number of frames needed before no repaint is
      requested and animations settle, and errors past `with_max_steps`
      (default 4). `assert_eq!(harness.run(), 2)` catches an accidental
      `request_repaint` loop or a layout that never settles — the usual reason
      an egui app pins a CPU core. It is deterministic and free.

      Deliberately **not** a wall-clock frame-time assertion. This project has
      already learned that timing assertions on shared runners are flaky; the
      whole `measure.sh` apparatus exists because of it. Track Criterion over
      `run_ui` and `tessellate` separately, alert on ratio rather than absolute,
      and keep any absolute budget on a dedicated machine.
- [ ] **Two or three Playwright smoke tests, no more.** App boots, wasm loads, a
      solve completes. Drive real events at canvas coordinates — eframe does
      listen on the canvas, so `page.mouse.click` produces genuine input — and
      assert through a `#[cfg(feature = "test_hooks")]` `#[wasm_bindgen]` state
      accessor reached via `WebRunner::app_mut`, never by pixel-matching.
      Export widget rects by stable test id so tests ask "where is
      `solve_button`" rather than hard-coding coordinates.
- [ ] **A golden-path integration test** that loads a real PGLib case, edits it,
      solves it, and checks the answer against the same case solved through the
      CLI. That single test catches nearly every plumbing regression.
- [ ] **`egui_mcp`, new in 0.35, is worth knowing about for this project
      specifically.** It exposes a running egui app's AccessKit tree over an
      inspection protocol and accepts synthetic events — launched with
      `EGUI_INSPECTION=1`, listening on port 5719 — with an MCP server so a
      coding agent can drive the app and take screenshots. Given how much of
      this repository's work is agent-assisted, an agent that can actually
      operate the UI and look at it is a meaningfully better feedback loop than
      one that can only read the source.
- [ ] Gotchas already paid for, recorded so nobody pays twice: `kittest.toml`
      uses `[mac]`, and `[macos]` **panics** under `deny_unknown_fields`;
      `wasm-bindgen-cli` in CI must match the `Cargo.lock` version exactly or
      failures are cryptic; `actions/checkout` needs `lfs: true` or snapshots
      fail with `InvalidSignature`; and `with_max_steps` defaults to 4, so
      anything with a longer animation or an async load needs raising it.

### Hosting

- [ ] Static deploy on Vercel; verified working for this exact shape, including
      free tier, static-only output, and `.wasm` served as `application/wasm`
      with Brotli, without configuration.
- [ ] `vercel.json` with `"source": "/(.*)"` — note Vercel's own knowledge-base
      example uses `"/"`, which matches only the root and misses workers and
      wasm under any other path.
- [ ] Two things that bite: Brotli compression removes `Content-Length`, so a
      byte-accurate download progress bar needs the expected size hardcoded or
      `Content-Encoding`-aware handling; and the Hobby tier's 100 MB static
      upload ceiling should be tested against rather than assumed, since the
      wasm bundle plus example networks could approach it.
- [ ] Cross-origin isolation is available on Vercel if threads are ever wanted —
      verified on live deployments, including a Rust app using rayon with shared
      memory. Use `require-corp` rather than `credentialless`: desktop Safari
      does not support the latter, and a self-contained studio has little
      third-party content for `credentialless` to rescue. Not needed for v1.
- [ ] One consequence of `COOP: same-origin` worth knowing before it surprises
      someone: it severs `window.opener`, which breaks popup-based OAuth and
      payment flows. Irrelevant while there is no sign-in, and a real constraint
      the day there is one.

## What the scaling measurements established

Worth writing down, because one of them cuts against this project's own
headline and it is better to have that on record than to discover it later.

Full year at hourly resolution, synthetic ring, solved whole:

**n = 5.** Every observation is listed rather than reduced to one number, for
reasons the sweep that produced it demonstrated twice over and which are set out
below.

| Buses | Columns | Build | Solve, all five (s) | Median | Growth |
| --- | --- | --- | --- | --- | --- |
| 8 | 402,960 | 4.4 ms | 3.4, 3.4, 3.5, 3.6, 3.8 | **3.5 s** | |
| 16 | 805,920 | 5.2 ms | 9.9, 10.5, 10.7, 10.9, 11.1 | **10.7 s** | 3.1× |
| 32 | 1,611,840 | 8.6 ms | 29.4, 30.0, 30.1, 31.8, 32.7 | **30.1 s** | 2.8× |
| 64 | 3,223,680 | 15.5 ms | 104.9, 106.8, 107.1, 109.5, 117.6 | **107.1 s** | 3.6× |
| 128 | 6,447,360 | 27.9 ms | 331.6, 335.8, 339.1, 358.0, 365.6 | **339.1 s** | 3.2× |

Build is the median of five; the column is millisecond-scale and noisy enough
that only its order of magnitude should be read (see the first-run note below).

The busy-machine version of this table put 128 buses at 561.8 s, which was 79%
high, and made growth look like it accelerated to 5× at the top when it is flat
at about 3×.

**Three things this table got wrong before n was five, all in the same
direction.**

*The numbers were low.* Everything above was previously quoted as best-of-two,
and best-of-N reports a floor: the median is 3 to 13% above it on every rung
(3.2 → 3.5, 9.5 → 10.7, 28.4 → 30.1, 103.8 → 107.1, 318.0 → 339.1). Best-of-N is
the right statistic for comparing two implementations, because taking the
fastest of each strips noise belonging to neither. It is the wrong one for "what
will this cost me". This document had been using one number for both questions.

*The spread was understated twice.* It was first published as 1.4%, then
corrected to 3–7% from two samples. At n=5 it is **11 to 12%** on every rung.

*And the explanation offered for the spread was invented.* The previous revision
claimed the rolling ladder below repeated to within 0.05 to 0.2% and reasoned
that one enormous factorisation varies where a hundred and twenty-two small ones
average out. That reasoning was built on two runs that returned 8.059901959 s
and 8.059746 s — a 0.002% agreement on an eight-second measurement, which is a
coincidence and not a distribution. At n=5 the rolling ladder's spread is 5.5 to
8.9%, so the mechanism is not merely unsupported, its conclusion is backwards.
**n=2 cannot distinguish a tight distribution from a lucky pair**, and the
temptation on seeing one is to supply a mechanism for it.

**A first-run penalty exists and it is in construction, not solving.** Run one
was the slowest observation on three of the six ladders re-measured, and the
effect scales with model size: building `case2869_pegase` — 68 M rows, 13 GB —
takes 1.5 s on the first run against a 421 ms median, 3.6×. Solves show no such
bias, even solves as short as 3.9 ms, so this is page-faulting freshly allocated
matrices rather than process warm-up in general. Median-of-five is robust to it
by construction, which is why the warm-up can stay off; a *mean* of five would
not be, and would read 51% high on that row.

**Re-measured to completion, best of two runs, and the previous version of this
table was wrong in every row.** It reported 64 buses as "did not finish in seven
minutes"; it solves in under two. 128 was never attempted and solves in nine and
a half. It reported 20 s at 16 buses against 10.3, and 194 s at 32 against 31.0.
It overstated the variable counts by about a quarter. It put growth at a flat
9.5× per doubling; it is 3× rising to 5×, a different shape as well as a
different number.

The rolling table below re-measured to within 6% of its published values, so
this was specific rather than general. Most likely machine load, the third time
in this project that a loaded machine produced a number that survived into a
document, after the transpose that "took 90 ms" and takes 21, and the prefault
experiment that measured backwards.

**The standard this now sets**, which is stricter than the one it replaces:
every timing is n=5 on an idle machine, every observation is printed rather than
summarised, the quoted figure is the **median**, and the sample size is stated
beside it. A reader can compute whatever summary they want from five values and
can recover nothing from one. Anything that cannot be re-measured should be
treated as suspect rather than quoted.

One caveat on the table above, recorded rather than buried. The load climbed
across this ladder's five runs — 4.98, 5.09, 4.68, 5.53, 5.67 — and runs four
and five are visibly the slow ones at 64 and 128 buses. `measure.sh` gates at
launch and cannot re-gate mid-run, so those two observations are upper bounds
and the medians above are, if anything, slightly high. That is the opposite of
the bias this section spent the day correcting, and it is worth one confirming
pass at a quieter moment before these numbers travel anywhere else.

The conclusion is unchanged: construction is 0.008% of runtime at 128 buses, so
**at full resolution the fast builder buys almost nothing**. That never rested on
the solve being intractable.

The same year through a rolling horizon of 96-hour windows keeping 72:

**n = 5**, same session and same machine as the whole-horizon table above, so
the two columns are comparable rather than assembled from different days.

| Buses | Windows | Rolling, all five (s) | Median | Whole (median) | |
| --- | --- | --- | --- | --- | --- |
| 16 | 122 | 3.92, 3.92, 3.95, 3.99, 4.14 | **3.95 s** | 10.7 s | 2.7× |
| 32 | 122 | 8.08, 8.30, 8.35, 8.36, 8.69 | **8.35 s** | 30.1 s | 3.6× |
| 64 | 122 | 21.82, 22.55, 22.77, 23.60, 23.63 | **22.77 s** | 107.1 s | 4.7× |
| 128 | 122 | 68.68, 68.73, 72.28, 73.62, 74.77 | **72.28 s** | 339.1 s | 4.7× |

The previous claim, 23× at 32 buses and that rolling "finishes at 64 and 128
where the monolithic solve does not", was an artefact of the bad table above.
It is 3.6× at 32 buses, and the monolithic solve finishes at every size tried.

Worth noting what survived the move to n=5: the first three rows land on 2.7×,
3.6× and 4.7×, against the 2.6×, 3.6× and 4.7× originally published. Those rows
were right all along. Only the 128-bus row was ever wrong, and it was wrong
because of the one figure in it that came from a loaded machine.

**The claim that replaced it was also wrong, and in the same way.** This table
read 7.4× at 128 buses against a "solved whole" column of 561.8 s — the exact
figure the table above it identifies as 79% high and discards. Two tables on one
page contradicted each other, and the headline ratio was taken from the
discredited one. Both columns now come from the same session on the same idle
machine.

What survives is weaker than what was claimed. On n=5 medians the advantage
**compounds to 64 buses and then flattens**: 2.7×, 3.6×, 4.7×, 4.7×. It does not
reach 7.4× and nothing here suggests it keeps climbing. Rolling still wins by
nearly five times at the top, which is the honest version, and it is still a
claim about decomposition rather than about this builder.

Worth recording why this one is different in kind. It is not a fifth bad
measurement; it is a **bad measurement that had already been caught**. The
561.8 s was identified, labelled and superseded twelve lines higher up, and the
ratio derived from it was left standing. Discrediting a figure does not
discredit what was computed from it, and nothing in this document was checking
for that.



Note what that does to the argument. Rolling performs **122 builds instead of
one**, and construction still does not register. So the build speed is not what
makes this scale; decomposition is. What the fast builder actually buys is
interactive rebuild, scenario sweeps where the same network is assembled
hundreds of times, and the memory ceiling. Those are real and they are a
narrower claim than "construction is the bottleneck".

- [x] ~~Pin the thread count to the performance cores on Apple Silicon.~~
      **Measured and rejected.** An M3 Max has ten performance cores and four
      efficiency ones at roughly a third of the speed, so the obvious worry is
      that the slow cores become stragglers in a work-stealing loop. Over five
      runs each, ten threads and fourteen are indistinguishable: medians of
      103.9 ms and 104.1 ms. The work is fine-grained enough that the efficiency
      cores simply take fewer chunks. Recorded because it is a plausible
      optimisation that measurement refuses, and adding the knob anyway would
      have been a knob nobody needs.
- [x] ~~Re-run these on real networks rather than a synthetic ring, since the
      ring's regular topology may flatter the solve.~~ **Done, and it does.**

      Real PGLib topologies carrying 2019 hourly demand and weather from the
      four German control zones, each against a synthetic ring sized to the same
      column count and measured in the same process minutes apart, so the pair
      differs in what is modelled rather than in what the machine was doing.

      **n = 2, and this is the one core table not on the n=5 standard the
      scaling section now sets.** The test takes the better of two passes per
      rung and prints its own spread column, which read 0 to 1% on the solve
      figures. Read that spread with the caution this document earned the hard
      way: two agreeing samples were twice mistaken today for a tight
      distribution, on the rolling ladder and on the simplex ladder, and both
      turned out to be coincidences that n=5 dissolved. So 0 to 1% here is not
      evidence of precision, it is an absence of evidence either way.

      The reason it stayed at two is cost: one pass is fifty minutes, so n=5 is
      over four hours. It is deferred rather than skipped, and until it is run
      the ratios below should be read to one significant figure.

      | Case | Columns | Real solve | Ring solve | Ratio | nnz/col real | nnz/col ring |
      | --- | --- | --- | --- | --- | --- | --- |
      | case14_ieee | 543,120 | 7.2 s | 5.7 s | 1.27× | 2.21 | 1.66 |
      | case57_ieee | 2,049,840 | 305.3 s | 42.0 s | **7.27×** | 2.26 | 1.65 |
      | case118_ieee | 4,826,760 | 787.3 s | 191.6 s | **4.11×** | 2.27 | 1.65 |

      So the ring has been flattering the solve by between 1.3 and 7 times at
      matched column counts, and the visible structural difference is density:
      a real network carries about 2.25 nonzeros per column against the ring's
      1.65. A ring has degree two everywhere; a transmission network has hubs,
      spurs and parallel circuits, and its bases fill in.

      The ratio is not monotonic, and two rows carry a caveat the test raises
      itself. It records the highest one-minute load seen per row, and flags
      anything above two as an upper bound rather than a measurement: the
      `ring 40` row ran at 6.8 and `case118_ieee` at 4.9, both self-inflicted by
      the solve. So 7.27× may understate and 4.11× may overstate, and the safe
      reading is "several times", not a curve.

      **The more useful finding is what it does to the decomposition argument.**
      The rolling horizon's advantage is far larger on real topologies than the
      ring ever suggested:

      | Case | Real rolling | Real whole | Advantage | Ring rolling | Ring whole | Advantage |
      | --- | --- | --- | --- | --- | --- | --- |
      | case14_ieee | 2.8 s | 7.2 s | 2.55× | 2.5 s | 5.7 s | 2.31× |
      | case57_ieee | 18.2 s | 305.3 s | **16.73×** | 11.0 s | 42.0 s | 3.80× |
      | case118_ieee | 74.7 s | 787.3 s | **10.53×** | 41.6 s | 191.6 s | 4.61× |

      Decomposition is worth two to five times on a ring and ten to seventeen on
      a real network. Every rolling-horizon number this project published from
      ring measurements therefore **understated** the case, which is the more
      pleasant direction for a correction to run.
- [x] ~~Measure where the rolling horizon's window length stops paying.~~
      **Done**, and the answer is a cliff rather than a curve.

      Measured on a system whose storage genuinely couples distant snapshots:
      wind arriving in multi-day spells against a reservoir holding a week, so
      that a window shorter than a calm spell cannot see across one. The
      reference is the same horizon solved whole, so every penalty is against a
      true optimum rather than against another approximation.

      | Window | Kept | Cost penalty |
      | --- | --- | --- |
      | 24 h | 12 | 189% |
      | 48 h | 24 | 148% |
      | 72 h | 36 | 110% |
      | 96 h | 48 | 92% |
      | **120 h** | 60 | **0%** |
      | 168 h | 84 | 0.7% |
      | 240 h | 120 | 0% |

      Nothing between 92% and zero. The fixture's calms last five days, and a
      window of 120 hours is the first that spans one: below it the horizon
      commits to buying through a drought it cannot see, and above it there is
      nothing left to learn. So the rule is not "longer is better" but **the
      window must span the longest event the storage is there to ride out**, and
      past that point extra window length buys nothing at all.

      The other axis, which the `Horizon::new` convention fixes without
      justification, behaves the same way. At a fixed window of 168, keeping
      anything up to 120 costs nothing, and keeping 144 costs 91%: what matters
      is that the lookahead is long enough, not that it is half.

      One honest note on the timings: at this size the rolling solve is 1.6 to
      2.8 times *slower* than solving whole, because 504 snapshots is small
      enough to solve outright and decomposition is pure overhead there. The
      penalties above are about foresight and are the transferable part; the
      timings are not.

## Benchmarks and validation

- [x] ~~**The linopy head-to-head.**~~ **Measured**, at last, and it is the
      founding claim so it should have been measured first.

      Same model, same machine, same session, 256 buses over 8,760 snapshots,
      reaching the same 16.3M variables and 29.2M nonzeros.

      **This entry was wrong and is corrected here.** It described the benchmark
      as "linopy 0.9.0 used idiomatically (vectorised over xarray dimensions with
      incidence arrays, not Python loops)" and reported 0.096 s against 200.8 s
      on 1.95 GB against 22.4 GB. Vectorising over a dense incidence array *was
      the bug*: `(p * g_at).sum("gen")` materialises a
      generators-by-buses-by-snapshots intermediate to express a sum in which all
      but three terms per bus are zero. It looks like ordinary xarray and it is
      not what the library is for. Written the way PyPSA writes it — `groupby`
      for per-bus sums, indexed `sel` for a line's two ends — linopy produces the
      identical matrix about 130 times faster.

      | | gridwright | linopy 0.9.0, properly | linopy as originally scripted |
      | --- | --- | --- | --- |
      | Construction | **0.096 s** | 1.54 s | 200.8 s |
      | Peak memory | **1.50 GB** | 3.45 GB | 22.4 GB |

      So the honest figures are **about 15× on time and 2.3× on memory**, not
      2,000× and 11×. The conclusion drawn from the old numbers — "22 GB is where
      a laptop stops" — was drawn from the artefact and does not survive it. A
      two-to-four-times memory advantage is worth having and is not, on this
      model, the difference between running and not running.

      What does survive is the narrower claim: a purpose-built assembler beats a
      general-purpose algebraic modelling layer by one to two orders of
      magnitude, and both general-purpose layers measured (linopy and JuMP) land
      within a factor of two of each other. That is a claim about the shape of
      the tool rather than about the language, and the founding quote was about
      the language.

      Still outstanding: these were taken on a machine that was not idle, with
      repetition counts varying by condition and no spread reported. They have
      not been re-run under `measure.sh` at n=5.

- [x] ~~Extend the differential harness to every constraint family.~~ **Done**,
      and it paid for itself on the first run by finding a formulation bug that
      every existing test passed over.

      `differential.rs` compares the two solvers on whole models, which is the
      right shape for asking whether the solver is correct and the wrong shape
      for asking *which* family a disagreement came from. `differential_families.rs`
      turns one family on at a time against the same base network: ramps,
      losses, carbon, water and land budgets, reserve margin, N-1, shiftable
      demand, elastic demand, hydro cascades, sector coupling, investment
      periods, scenarios, taps and phase shifts, and capacity expansion.

      Each test asserts two things, and the second is the one that earns its
      keep: the solvers agree, **and the family changed the answer**. A test
      that enables a constraint which does not bind proves only that both
      solvers can ignore it consistently. Four of the sixteen failed on the first run for exactly that reason (my fixtures, not the code),
      and fixing them is what surfaced the real bug below.

      **The bug.** Hydro cascades were a separate row family,
      `soc_downstream[arrival] >= released`, sitting beside the downstream
      reservoir's own state-of-charge *equality*. An equality already pins
      `soc`, so that row could not hand over any water; it could only demand
      the downstream reservoir be fuller than its own dynamics made it, and the
      only way to comply is to charge from the grid. An upstream release
      therefore made the system **buy energy** rather than receive water: on a
      two-reservoir probe, coupling the cascade cost 6,000 against 0 uncoupled,
      with the lower reservoir charging 40 MWh off a diesel unit.

      Arriving water now enters the downstream balance as a term in it, exactly
      as natural inflow does but as a decision rather than a constant, and the
      separate family is gone. The same probe now shows the upper reservoir
      spilling and the lower one discharging what came down, at no cost.

      It survived because the existing test asserted only that both cases solve.
      Its comment claimed "the diesel has to run and the cost is far higher",
      an assertion that was described and never written.
- [x] ~~Larger real networks.~~ **Partly done.** PEGASE 1354 is now in the
      validation suite: a real European network, four times the largest IEEE
      case. Every property test holds on it, the from-scratch simplex agrees
      with HiGHS on its objective, and the AC relaxation solves it with every
      voltage inside its band. The remaining size question is the same one as
      before, and it is about time series rather than topology.
- [x] ~~Larger still: PGLib has cases up to 13,659 buses.~~ **Done.** PEGASE
      2869, RTE 6470, PEGASE 9241, PEGASE 13659 and GOC 2000 are in the suite,
      2.45 MB compressed. Every physical property is checked per bus rather than
      in aggregate: nodal balance at every bus, flows within ratings, dispatch
      within limits, the DC flow equation on every branch, one pinned angle per
      synchronous area, and an identical matrix from building twice.

      Construction is essentially linear and never above 7 ms. The 13,659-bus
      network builds faster than the 1,354-bus one solves.

      Medians of **n = 5**, except `case1354_pegase` which is carried over from
      an earlier single pass and is the one figure here not taken that way.

      | case | buses | rows | cols | nonzeros | build | solve |
      | --- | --- | --- | --- | --- | --- | --- |
      | case1354_pegase | 1,354 | 3,345 | 4,959 | 11,569 | 1.6 ms | 51 ms |
      | case2869_pegase | 2,869 | 7,451 | 10,830 | 26,289 | 3.8 ms | 238 ms |
      | case6470_rte | 6,470 | 15,395 | 22,706 | 52,016 | 3.7 ms | 737 ms |
      | case9241_pegase | 9,241 | 25,274 | 35,976 | 90,883 | 5.3 ms | see below |
      | case13659_pegase | 13,659 | 34,110 | 51,877 | 120,038 | 6.0 ms | 6.6 s |

      The solve column is worth its digits: 3.6 to 4.0% spread across five runs
      at the two smaller sizes and 1.5% at the largest. The build column is not.
      Its five observations at `case13659_pegase` were 9.5, 6.0, 6.4, 5.9 and
      5.7 ms — a 67% range in which the first run is the outlier — so read it as
      "a few milliseconds" and not as three significant figures.

      **HiGHS 1.15.0 cannot solve case9241_pegase, and the from-scratch simplex
      can.** `Highs_run` returns an error after three to nine seconds with model
      status "Not Set", deterministically, unaffected by thread count. It is not
      the model: no infinite or NaN coefficient, no NaN row bound, presolve
      reduces it cleanly, and PEGASE 13659, a larger model of the same system
      from the same publisher with the same susceptance range, solves fine. Ours
      solves it to optimality and the answer passes every physical check.

      That is the strongest argument this project has for having written its own
      solver, and it was found by accident rather than looked for. The reason
      the solver exists was that HiGHS cannot go in *this* browser module —
      it compiles to wasm perfectly well through Emscripten, as `highs-js`
      demonstrates, but that is `wasm32-unknown-emscripten` and will not link
      into a `wasm32-unknown-unknown` Rust binary. That it *also*
      solves a case HiGHS declines is a better reason and nobody predicted it.
      Three tests pin it, including one asserting HiGHS still fails, to be
      deleted when an upgrade fixes it.

      **The synthetic ring has been flattering the numbers**, which the item
      below suspected and this settles. The simplex runs at about `rows^2.5` on
      real topologies against the `rows^1.9` the ring gives. HiGHS runs the same
      ladder at about `rows^2.1`, so it is partly the problem and partly us.
- [x] ~~**Re-run the large-case simplex ladder on an idle machine.**~~ **Done**,
      and the drift was entirely load. **n = 5:**

      | Case | Rows | Ours, all five (s) | Median | HiGHS (median) | Was |
      | --- | --- | --- | --- | --- | --- |
      | case2869_pegase | 7,451 | 2.7, 2.7, 2.7, 2.7, 2.8 | **2.7 s** | 243 ms | 2.89 / 2.9 s |
      | case6470_rte | 15,395 | 23.5, 23.6, 23.7, 24.8, 24.9 | **23.7 s** | 760 ms | 23.5 / **42** s |
      | case13659_pegase | 34,110 | 173.2, 173.5, 175.2, 175.9, 186.9 | **175.2 s** | 7.1 s | 197 / **230** s |

      The two contaminated passes read 42 s and 230 s; both are junk. The
      *better* of the two earlier passes was also high — 197 s against a 175.2 s
      median, so 12% over — which is the part worth remembering. The instinct on
      seeing two disagreeing passes is to trust the faster one, and here the
      faster one was still wrong.

      **This entry previously claimed a run-to-run spread "under 0.5%" and that
      was an artefact of n=2.** The two samples were 167.6 and 167.5 s, and a
      0.06% agreement reads as a tight distribution when it is a coincidence. At
      n=5 the spread is **3.7 to 7.9%** on ours and up to **19%** on HiGHS, whose
      case13659 observations run 6.8 to 8.1 s. The same mistake was made
      independently on the rolling ladder in the same session; both are recorded
      because one instance looks like bad luck and two look like a method.

      Run one is the slowest observation on both large rungs — 186.9 s against a
      175.2 s median, and HiGHS 8.1 s against 7.1 s — consistent with the
      first-run effect described in the scaling section.

      The exponent survives. Over these three rungs the from-scratch solver runs
      at `rows^2.5` to `rows^3.0` on medians, against the `rows^2.5` claimed from
      the contaminated ladder; HiGHS runs the same rungs at about `rows^2.1`,
      unchanged. The conclusion is unchanged with it: the difficulty is partly
      the problem and partly us, and three points still cannot settle how it
      divides.

- [ ] **The `area` column blocked more than the GOC cases, and may still be
      blocking something.** Reading MATPOWER's control areas as synchronous
      areas is fixed, but the same question applies to every other reader that
      carries an area concept: PSS/E has one, CIM has control areas, and UCTE
      puts a country letter in the node code. Each should be checked against the
      same standard, that a synchronous area is a connected component of the AC
      network and not a label.
