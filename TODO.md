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
      the curve is a shallow U — 3.55 s at 32 pivots, 3.30 at 64, 3.16 at 256,
      3.20 at 512, and 5.41 s never refactorising at all. Fitting
      `base + A/k + B·k` gives a base of 3.03 s, and at the optimum the variable
      terms are 0.06 s of refactorisation against 0.07 s of update application.

      Applying the updates is **2.3% of runtime**. That is the entire addressable
      cost, and Forrest-Tomlin would replace it with something rather than
      nothing. The other 96% is triangular solves and pricing, which it does not
      touch. The earlier suspicion that eta growth explained the `m^2.5` tail
      was wrong: that was the factorisation scan, fixed by the symbolic search.

      The measurement paid for itself anyway — the default interval moved from
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
- [ ] **A structural crash**, which is the version that would help. Rather than
      choosing bounds, choose a starting *basis*: a triangular selection of
      structural columns instead of the all-artificial one, so phase one starts
      much nearer feasible. Harder, because a badly chosen basis is singular and
      the selection has to guarantee it is not.
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

      Verified against HiGHS on a commitment problem constructed so its
      relaxation is provably fractional — many commitment relaxations come out
      integral on their own, and a test built on one of those exercises the
      branching not at all.
- [ ] Better branching than most-fractional. Pseudo-cost branching uses the
      history of how much each variable's bound actually moved the objective,
      which most-fractional ignores entirely. The difference shows on problems
      larger than a page will run, which is why it was not done first.
- [ ] Cuts. Nothing is added at a node beyond the branching bounds, so the
      search explores territory that a Gomory or cover cut would have removed
      outright.
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

      Water accrues per megawatt-hour generated, which is what makes it bind in
      exactly the weeks demand peaks — in much of the world cooling water rather
      than carbon decides whether a station runs through a dry summer. Land
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
- [ ] **Head's effect on energy conversion**, as opposed to on available
      capacity, which is implemented. A full reservoir yields more megawatt-hours
      from the same volume of water, because the water falls further. That part
      is bilinear in flow and volume rather than linear, so unlike the capacity
      effect it cannot go into the LP as written. It needs either a piecewise
      linearisation over head bands or the same convex-envelope treatment the
      AC relaxation uses — and the machinery for the second now exists, in
      `gridwright-acopf::bnb`.
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
- [x] ~~Bus shunt admittances.~~ **Done.** A conductance draws real power that
      somebody has to generate — case300 carries over a megawatt of it — and a
      susceptance injects the reactive power capacitor banks exist to supply.
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
      variables. A square limit would have allowed the corner — √2 times the
      rating — which is what a pair of linear bounds carried over from the DC
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
      not have to sort the curve, and demand beyond what the curve covers still
      falls back on the value of lost load — a curve says what a consumer will
      pay, not that they are indifferent past its end.
- [x] ~~**Interruptible contracts.**~~ **Done.** A binary per snapshot saying
      whether the contract was called, the energy not delivered bounded by both
      the contract's size and that binary, and the calls counted over the
      horizon against the agreed number.

      That count is the whole contract: without it this is expensive shedding
      with extra steps, and it is also the only part that cannot be written
      linearly, which is why a contract makes the model an integer one.

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
      archives. Both formats that are zips — a spreadsheet and a CGMES model —
      are told apart by what is inside rather than by the extension, because a
      CGMES archive is as likely to be named for its operator as for its
      contents.
- [x] ~~**The SSH profile** of a CGMES model.~~ **Done.** The equipment profile
      describes plant and the hypothesis says what it is doing, so a reader that
      stops at equipment produces a network with correct topology and no load in
      it, which solves and means nothing. Set points and in-service flags are
      both honoured, and a model built as designed rather than as operated is a
      different and usually more capable network.
- [ ] **The SV profile**, which carries a solved state: voltages, angles and
      flows. Not needed to build a model and extremely useful to validate one
      against, since it is the answer the operator's own tools produced.
- [x] ~~**PSS/E `.rawx`**, the JSON reformulation v35 introduced.~~ **Done**,
      and it is a fraction of the length of the RAW reader for a reason: RAWX
      names every field where RAW is positional, so the whole class of
      version-dependent column offsets simply does not arise. Three JSON
      dialects now share the extension and are told apart by content.
- [ ] **UCTE-DEF and IEEE Common Data Format.** Both are fixed-width text, both
      are how a lot of *historical* European and American data is archived. Less
      pressing than the live formats, and the reason to want them is that
      studies reaching back decades cannot use anything else.
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
      `DIMENSION_LIST` and `REFERENCE_LIST` attributes carrying object
      references — file offsets not known until the file has been laid out. The
      pure-Rust HDF5 library exposes the reference datatype and no way to emit
      one, and a `.nc` xarray refuses to open is worse than no `.nc`.
      `import_from_csv_folder` reaches the same destination by a road that
      exists.

      The conversion that matters runs the opposite way from the reader's: PyPSA
      states impedance in ohms, so per unit has to be undone against the base
      voltage. Writing per-unit values into a field PyPSA reads as ohms gives
      lines that are very nearly short circuits, which does not fail — it
      produces answers.
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
path, and a network round-trips losslessly through JSON. What remains is the
binding layer, the interface itself, and one piece of solver work that decides
what the interface can honestly offer.

- [ ] **Sparse LU first.** See the solver section. At 864 rows the in-page
      solver takes a second, and that is the whole constraint on what a browser
      build can do unaided.
- [ ] **`wasm-bindgen` wrapper**: load a file, edit a network, solve, read
      results, all across the JavaScript boundary. Small, and blocked on
      nothing.
- [ ] **Decide where the solve happens.** Two honest designs, and the choice
      changes the product rather than the plumbing. Either the page edits and
      visualises while a server runs HiGHS, which handles real studies and needs
      a server; or everything runs in the page, which needs nothing and is
      bounded by the paragraph above. Both are defensible and doing one well
      beats hedging.
- [ ] Rust GUI (Dioxus) importing the engine as a library, compiled to WASM for
      the browser and natively for desktop.
- [ ] Network editing with live rebuild. Construction being fast is worth most
      here rather than in batch: an edit that rebuilds in a hundred milliseconds
      is a different interaction from one that takes a second.
- [ ] Result visualisation: flows on a map, price duration curves, dispatch
      stacks, capacity build-out by period.
- [ ] Show the *status* of an answer, not just the number. An AC result that is
      a relaxation rather than an operating point, a head iteration that did not
      converge, and a branch-and-bound that stopped on its node limit are all
      things the engine now reports and an interface would be wrong to hide.

## What the scaling measurements established

Worth writing down, because one of them cuts against this project's own
headline and it is better to have that on record than to discover it later.

Full year at hourly resolution, synthetic ring, solved whole:

| Buses | Variables | Build | Solve |
| --- | --- | --- | --- |
| 16 | 1.0M | 11 ms | 20 s |
| 32 | 2.0M | 14 ms | 194 s |
| 64 | 4.1M | — | did not finish in seven minutes |

The solve grows about 9.5× for a doubling, and construction is 0.0–0.1% of
runtime. **At full resolution the fast builder buys almost nothing**, which the
README has always said and which is now measured rather than asserted.

The same year through a rolling horizon of 96-hour windows keeping 72:

| Buses | Windows | Total |
| --- | --- | --- |
| 16 | 122 | 4.0 s |
| 32 | 122 | 8.5 s |
| 64 | 122 | 23 s |
| 128 | 122 | 72 s |

Twenty-three times faster than solving whole at 32 buses, and it finishes at 64
and 128 where the monolithic solve does not. Scaling falls to roughly
`O(n^1.6)`.

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
- [ ] Re-run these on real networks rather than a synthetic ring once a large
      case with a full year of time series is assembled, since the ring's
      regular topology may flatter the solve.
- [ ] Measure where the rolling horizon's window length stops paying: shorter
      windows solve faster and lose more foresight, and nobody has established
      the trade on a case with storage that matters.

## Benchmarks and validation

- [ ] **The linopy head-to-head.** Still unmeasured, still the thing that
      decides whether the performance claim survives contact with the
      incumbent. The README says so plainly and should keep saying so until it
      is done.
- [ ] Extend the differential harness to every constraint family. It caught the
      phase-one bug immediately and is the highest-value test infrastructure in
      the repository.
- [x] ~~Larger real networks.~~ **Partly done.** PEGASE 1354 is now in the
      validation suite: a real European network, four times the largest IEEE
      case. Every property test holds on it, the from-scratch simplex agrees
      with HiGHS on its objective, and the AC relaxation solves it with every
      voltage inside its band. The remaining size question is the same one as
      before, and it is about time series rather than topology.
- [ ] Larger still: PGLib has cases up to 13,659 buses, and RTE and
      PEGASE are in the same format we already read.
