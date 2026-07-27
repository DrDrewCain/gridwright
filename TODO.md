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
      crashes a basis, runs phase one and then phase two, every time. Phase one
      is about three quarters of the iterations of a solve — 33,670 of 45,205
      at 20,736 rows — and for a child node it is entirely wasted, because the
      parent's basis was already feasible for everything except the one bound
      that changed.

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
comfortable in-page today. A continental year is not, and no amount of frontend
work makes it one.

### The architecture this settles on

```
crates/
  gridwright-*        the engine, unchanged, no UI dependencies
  gridwright-studio/  eframe app: docking, network editor, charts, 3D view
  gridwright-worker/  [[bin]] -> its own .wasm, wraps the engine,
                      receives a model, streams progress, returns results
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

### Stage 2 — the studio shell

- [ ] `egui_tiles` docking: a viewport, an inspector, a run/console panel, and
      a scenario browser, all rearrangeable and persisted between sessions.
- [ ] A command palette. It is the cheapest discoverability mechanism there is
      and it makes every later feature findable without menu archaeology.
- [ ] Undo/redo as an explicit command stack over model edits. Retrofitting undo
      into a canvas editor is a rewrite; designing it in on day one is a trait.
- [ ] Project save/load, and autosave into OPFS. OPFS is Baseline-wide since
      2023; the File System Access API is Chromium-only in 2026 and must be a
      progressive enhancement, never the load-bearing path.

### Stage 3 — the network editor, which is the actual product

- [ ] Canvas with pan/zoom, marquee select, snapping, and a minimap. Rendered
      through `wgpu` in an egui panel so it stays smooth at thousands of nodes.
- [ ] Node and edge editing: buses, lines, generators, loads, storage. Typed
      inspectors generated from the model types where possible.
- [ ] **Live rebuild on edit.** This is the thesis the whole engine rests on and
      it is still, per gap 4 in the README, untested as a workflow. The
      measurements say it holds: an edit at regional scale rebuilds in single
      -digit milliseconds in the browser. Prove it with a real edit loop.
- [ ] Geographic layout when coordinates exist, force-directed when they do not.
- [ ] Large-network behaviour: level-of-detail, culling, and a decision about
      what a 13,659-bus network even looks like on screen.

### Stage 4 — results, and being honest about them

- [ ] Flows on the network, coloured by loading, animated by direction.
- [ ] Dispatch stacks, price duration curves, storage state of charge,
      capacity build-out by period. `egui_plot` for all of it.
- [ ] Nodal price heatmap on the network itself — this is the output the engine
      exists to produce and the one competing browser tools cannot show.
- [ ] **Show the *status* of an answer, not just the number.** An AC result that
      is a relaxation rather than an operating point, a head iteration that did
      not converge, and a branch and bound that stopped on its node limit are
      all things the engine reports and an interface would be wrong to hide. A
      result with an `OPEN` gap must look different from a proved optimum.
- [ ] **Infeasibility diagnosis.** Reportedly the single largest pain point in
      every incumbent tool. "Infeasible" with no further information is a dead
      end for a user; the engine should be able to say which constraints
      conflict. Needs solver work, not just UI.

### Stage 5 — scenarios, which is what makes it a studio

- [ ] Define a scenario as a diff against a base network, not a copy.
- [ ] Run a sweep and compare runs side by side.
- [ ] The comparison view: what changed, what it cost, which constraints bound.

### Testing the interface

Untested UI code rots faster than anything else in a codebase, and a canvas app
resists the usual tools: there is no DOM, so Playwright selectors have nothing
to grip.

- [ ] **`egui_kittest`** for widget and interaction tests — the official harness.
      Simulated clicks, typing and drags, running headless in CI.
- [ ] **Snapshot tests** on rendered frames for the editor and each chart type,
      with a software rasteriser path so results do not vary by GPU.
- [ ] **`wasm-bindgen-test` in headless Chrome and Firefox** for anything that
      only fails in the browser: worker round-trips, OPFS, file loading,
      memory-growth behaviour.
- [ ] **End-to-end through a test hook rather than the DOM.** Expose a small
      scripted-command surface from Rust so a browser test can drive the app and
      assert on model state, instead of pixel-matching a canvas.
- [ ] **Frame-time budget as a test.** A studio that drops below 60 fps while
      panning a large network has a bug, and it should fail CI rather than be
      noticed in a demo.
- [ ] **Accessibility assertions through AccessKit**, which egui already wires
      up. Canvas apps are the worst offenders here and it is much cheaper to
      keep it working than to retrofit it.
- [ ] **A golden-path integration test** that loads a real PGLib case, edits it,
      solves it, and checks the answer against the same case solved through the
      CLI. That single test would catch nearly every plumbing regression.

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
      it compiles to wasm perfectly well through Emscripten, as `highs-js` and
      `highs-wasm` both demonstrate, but that is `wasm32-unknown-emscripten` and
      will not link into a `wasm32-unknown-unknown` Rust binary. That it *also*
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
