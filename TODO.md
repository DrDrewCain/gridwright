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
- [ ] Water use and land use, on the same machinery as CO₂. Both are real
      constraints in siting decisions and neither needs new solver work: they
      are additional linear rows over the same dispatch variables.
- [ ] Emissions of the *network* itself: line losses are already modelled, and
      the carbon attributable to them currently lands on whoever consumed the
      power rather than being reported as a loss.

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
      AC relaxation uses. Until then a cascade's energy yield is understated at
      high storage and overstated at low.
- [ ] **Spatial branch and bound on the McCormick boxes.** This is the one that
      would close the AC relaxation gap properly. The envelopes are exact at the
      corners of a box and loose in its interior — deliberately, and provably —
      so tightening them means splitting the box and recursing, which is a
      branch-and-bound search over variable ranges rather than over integers.
      Everything else in the AC formulation is a lower bound waiting on this.
- [ ] **Cycle constraints for fundamental cycles longer than three.** Same
      identity, more factors: the imaginary part of a product around a loop
      still has to vanish, but the envelope tower grows with the cycle length
      and the auxiliary variables multiply faster than the tightening repays.
      Triangles are done. Longer cycles want the branch and bound above first,
      since without it they add variables to a bound that is loose for a
      different reason.
- [ ] **Bus shunt admittances** (`Gs`, `Bs`), currently read and ignored. Zero
      on most test cases and emphatically not zero on real ones: reactive
      compensation is how voltage is actually held, and an AC answer that
      ignores it is answering about a network with no capacitor banks.
- [ ] **Transformer phase shift angles**, the other half of the branch model.
      Tap ratios are handled; the shift is not, so a phase-shifting transformer
      currently reads as an ordinary one and the flow it was installed to
      control is unconstrained.
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

- [ ] **CGMES in its published form**, which is a zip of profile files rather
      than a directory. Unzipping first works today; reading the zip directly
      does not.
- [ ] **The SSH and SV profiles** of a CGMES model. Equipment and topology are
      read; the steady-state hypothesis carries the actual load and generation
      set points, and without it a published model arrives with correct topology
      and no demand.
- [ ] **PSS/E `.rawx`**, the JSON reformulation of RAW that v35 introduced. The
      content is the same and the encoding is not.
- [ ] **UCTE-DEF and IEEE Common Data Format.** Both are fixed-width text, both
      are how a lot of *historical* European and American data is archived. Less
      pressing than the live formats, and the reason to want them is that
      studies reaching back decades cannot use anything else.
- [ ] **Writers for the formats that only read.** CSV, Parquet and JSON write;
      MATPOWER, PSS/E, netCDF and CIM do not. Converting *into* a format the
      rest of someone's toolchain speaks is half of what an import layer is for,
      and right now the arrows only point one way.
- [ ] **Streaming the large time series.** A year of hourly data for a
      continental model is read whole into memory. Parquet is chunked on disk
      and the reader already walks it in batches, so bounding memory is a matter
      of not accumulating, not of new machinery.

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
