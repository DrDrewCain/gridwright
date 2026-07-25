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
- [ ] Sparse LU with Forrest-Tomlin updates, replacing the dense `m × m`
      inverse. The dense basis caps us at a few thousand rows, and the
      differential suite already takes 33 seconds because of it. Correct but
      slow was the right first target; this is the second.
- [ ] Branch and bound, so the pure-Rust backend can do unit commitment. It
      currently declines MIPs rather than returning a relaxation, which is the
      right behaviour but a real limitation.
- [ ] Consider upstreaming a `row_duals()` accessor to microlp regardless. They
      already compute `y = B⁻ᵀc_B` in `recalc_obj_coeffs` and discard it, and the
      whole ecosystem would benefit.

## Emissions and footprinting

- [ ] **Carbon intensity of consumption, per bus per snapshot.** Different from
      what we have now. The CO₂ budget constrains *production*; what people are
      usually asked about is the intensity of the electricity a given node
      *consumed*, which depends on where its imports came from. Requires
      attributing flows back to their generating source through the network.
- [ ] **Average vs marginal emissions.** Both are legitimate and they answer
      different questions. Average intensity is total emissions over total
      generation. Marginal intensity is the emissions of the unit that would
      respond to one more MWh, which is what actually matters for deciding
      whether to shift a load. They can differ by a factor of two, and quoting
      the wrong one is a common and consequential error.
- [ ] **Embodied emissions.** A wind turbine emits nothing while running and a
      great deal while being built. A model that scores only operational
      emissions will over-build renewables relative to a full accounting.
      Needs a per-MW-built emissions figure alongside `capital_cost`.
- [ ] **Emissions by country and by carrier**, aggregated from what the solve
      already produces. Cheap once the attribution above exists.
- [ ] **Carbon price as an alternative to a cap.** A cap answers "what if we are
      not allowed to exceed X"; a price answers "what if emitting costs Y". The
      dual of the CO₂ row already gives the shadow price implied by a cap, so
      the two are connected and the connection is worth exposing.
- [ ] Water use and land use, on the same machinery as CO₂. Both are real
      constraints in siting decisions and neither needs new solver work: they
      are additional linear rows over the same dispatch variables.

## Formulation

- [ ] AC power flow via second-order cone relaxation. Needs a conic solver
      behind a third backend; `clarabel` is pure Rust and would compile to WASM
      alongside everything else. The README previously called this permanently
      out of scope, which was wrong and has been corrected.
- [x] ~~N-1 security constraints.~~ **Done.** Formulated through line outage
      distribution factors, so security costs rows rather than columns: the
      naive approach duplicates every flow variable per contingency, while LODF
      turns each outage into constraints on the base-case flows that already
      exist. Lines whose loss would island the network are reported rather than
      silently skipped. Verified by replaying every outage against the solved
      base case, and on IEEE 14 under full N-1.
- [ ] Hydro head effects, where a reservoir's output per unit of water depends
      on how full it is. Genuinely bilinear (power depends on flow *times*
      head), so it needs a concave piecewise approximation with tangent planes
      rather than a straight linearisation. The tangents are an upper bound, so
      the result is a relaxation and slightly optimistic, which is standard and
      should be labelled as such.
- [x] ~~Rolling horizon unit commitment.~~ **Done.** Overlapping windows with
      reservoir levels and commitment states carried across, since a window that
      assumes every unit starts cold invents start-up costs already paid.
      Checked against solving the same horizon whole, and that more lookahead
      never costs more.

## Interface

- [ ] Rust GUI (Dioxus) importing the engine as a library, compiled to WASM for
      the browser and natively for desktop. **Unblocked:** the pure-Rust solver
      returns prices and compiles to `wasm32-unknown-unknown` with one
      dependency.
- [ ] Network editing with live rebuild. The engine builds a 16M-variable model
      in ~100 ms, so editing a line and watching the model rebuild immediately
      is achievable and is a demo nothing else in this space can offer.
- [ ] Result visualisation: flows on a map, price duration curves, dispatch
      stacks, capacity build-out by period.

## Benchmarks and validation

- [ ] **The linopy head-to-head.** Still unmeasured, still the thing that
      decides whether the performance claim survives contact with the
      incumbent. The README says so plainly and should keep saying so until it
      is done.
- [ ] Extend the differential harness to every constraint family. It caught the
      phase-one bug immediately and is the highest-value test infrastructure in
      the repository.
- [ ] Larger real networks: PGLib has cases up to 13,659 buses, and RTE and
      PEGASE are in the same format we already read.
