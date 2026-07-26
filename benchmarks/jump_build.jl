# Build the same model in JuMP, and time only the building.
#
# This is the benchmark the project's founding quote actually asks for. The
# quote says Python is "non-competitive" at building optimisation problems
# "compared to tools based on Julia or C++". Measuring against linopy measures
# the tool the quote criticises. JuMP is the tool the quote holds up as the
# alternative, so this is the comparison that can falsify the premise rather
# than confirm it.
#
# Fairness, since a benchmark is only as good as its fairness:
#
# - JuMP is used the way its own performance guidance says to use it. That
#   means the macros rather than operator overloading, generator expressions
#   inside `@constraint` (which the macro lowers to in-place
#   `add_to_expression!` calls), `set_string_names_on_creation(model, false)`,
#   and `add_to_expression!` for the objective. JuMP has no numpy-style
#   broadcast construction and its authors do not recommend one: the indexed
#   macro *is* the vectorised path, and it compiles. Writing this with
#   `+=` on expressions, or with string names left on, would be a strawman.
#
# - The model is the same synthetic ring `gw bench` builds and the same one
#   `linopy_build.py` builds: nodal balance, DC flow with angles, storage
#   dynamics across snapshots, and shedding. Bounds and coefficients are
#   copied from `linopy_build.py` so the three scripts are literally the same
#   matrix rather than merely the same shape.
#
# - Counts are asserted, not assumed. gridwright reports 16,258,560 columns,
#   6,167,040 rows and 29,153,280 nonzeros at 256 buses over 8,760 hours. The
#   script recomputes all three analytically, checks them against what JuMP
#   reports, and prints a loud mismatch line if they disagree. This is not
#   ceremony: the linopy benchmark had a real bug caught exactly this way,
#   where a degenerate constraint gave linopy less to build and the nonzero
#   counts were what gave it away.
#
# - Storage is cyclic here, so the first snapshot links to the last and every
#   storage row has four nonzeros. gridwright's storage is cyclic
#   (`cyclic: true` in the CLI's synthetic network), so this matches it
#   exactly. `linopy_build.py` uses `.shift(snapshot=1)`, which drops the term
#   at the first snapshot, which is why its published nonzero count is 64
#   lower. JuMP is therefore asked to build 64 more nonzeros than linopy was,
#   not fewer. Pass `--no-cyclic` to reproduce linopy's exact count.
#
# - Only construction is timed. Nothing here is solved. Both engines hand the
#   result to the same solver afterwards and that part is not in dispute.
#
# - Julia compiles on first call, so a cold run measures the compiler. A small
#   warm-up build runs first through exactly the same functions with exactly
#   the same types, and only runs after it are timed. `--reps` then takes the
#   best of N, because the machine matters: a measurement in this project has
#   been corrupted by background load three separate times.
#
# Three backends, because "construction time" means different things and the
# honest thing is to report all of them rather than pick the flattering one:
#
#   --backend cached   The default `Model()`. This is what a JuMP user writes,
#                      and it builds into MOI's in-memory cache rather than
#                      into a solver's matrix. This is the headline number.
#   --backend direct   `direct_model(HiGHS.Optimizer())`, which JuMP documents
#                      as the way to skip the cache and build straight into the
#                      solver. This is the closest analogue of gridwright's
#                      figure, which is time to a matrix a solver takes.
#   --backend matrix   `cached`, then `lp_matrix_data` to extract a sparse
#                      matrix, timed separately. This mirrors linopy's separate
#                      export column. JuMP's own docs call `lp_matrix_data`
#                      pedagogical and say not to use it as a solver interface,
#                      so its cost is reported apart from construction and
#                      should not be read as JuMP's production path.
#
# Setup, if JuMP is not already present:
#
#   julia -e 'using Pkg; Pkg.add(["JuMP", "HiGHS"])'
#
# Run, with peak resident memory measured the same way as every other figure
# in this project:
#
#   /usr/bin/time -l julia benchmarks/jump_build.jl --buses 256 --hours 8760 \
#       --backend cached --reps 3
#
# Peak memory is only meaningful with `--reps 1`, since more repetitions leave
# the previous model live long enough to overlap with the next one.

using JuMP
import HiGHS
import SparseArrays
using Printf

# Bounds and coefficients, lifted verbatim from linopy_build.py so that the
# two scripts describe the same matrix and not merely the same shape.
const P_MAX = 400.0
const F_MAX = 500.0
const SOC_MAX = 600.0
const CH_MAX = 100.0
const SUSCEPTANCE = 10.0
const DEMAND = 300.0
const EFF = 0.94

"""
Adjacency for the synthetic network, precomputed once so that it is data setup
rather than construction. gridwright reports its own network generation
separately from construction for the same reason: neither engine should be
charged for building the inputs.
"""
struct Topology
    buses::Int
    hours::Int
    lines::Int
    gens::Int
    stores::Int
    bus0::Vector{Int}
    bus1::Vector{Int}
    gens_at::Vector{Vector{Int}}
    lines_from::Vector{Vector{Int}}
    lines_to::Vector{Vector{Int}}
    stores_at::Vector{Vector{Int}}
end

"""
The ring plus chords that `gw bench` generates: one ring line per bus, then a
chord from every second bus to the one a third of the way round. Three
generators per bus, storage on every fourth bus. The chord pattern is taken
from the CLI rather than from linopy_build.py, which used a simpler adjacency
with the same counts. Topology does not change any of the three counts, since
what matters is that every line has exactly two endpoints.
"""
function topology(buses::Int, hours::Int)
    gens = buses * 3
    stores = max(buses ÷ 4, 1)

    bus0 = Int[]
    bus1 = Int[]
    for b in 1:buses
        push!(bus0, b)
        push!(bus1, b % buses + 1)
    end
    if buses > 8
        for b in 0:2:(buses - 1)
            far = (b + buses ÷ 3) % buses
            if far != b
                push!(bus0, b + 1)
                push!(bus1, far + 1)
            end
        end
    end
    lines = length(bus0)

    gens_at = [Int[] for _ in 1:buses]
    for g in 1:gens
        push!(gens_at[(g - 1) % buses + 1], g)
    end

    lines_from = [Int[] for _ in 1:buses]
    lines_to = [Int[] for _ in 1:buses]
    for l in 1:lines
        push!(lines_from[bus0[l]], l)
        push!(lines_to[bus1[l]], l)
    end

    stores_at = [Int[] for _ in 1:buses]
    for s in 1:stores
        push!(stores_at[((s - 1) * 4) % buses + 1], s)
    end

    return Topology(buses, hours, lines, gens, stores,
                    bus0, bus1, gens_at, lines_from, lines_to, stores_at)
end

"""
What the matrix must come out to, derived from the topology rather than copied
from gridwright's output, so that agreement is evidence rather than assertion.

Per snapshot: a balance row carries every generator, both endpoints of every
line, a charge and a discharge term per store, and one shed term per bus. A DC
flow row carries the flow and the two angles. A storage row carries this
snapshot's state of charge, the previous one, the charge and the discharge.
"""
function expected_counts(t::Topology; cyclic::Bool)
    cols = (t.gens + t.lines + t.buses + t.buses + 3 * t.stores) * t.hours
    rows = (t.buses + t.lines + t.stores) * t.hours
    per_snapshot = (t.gens + 2 * t.lines + 2 * t.stores + t.buses) +
                   (3 * t.lines) +
                   (4 * t.stores)
    nnz = per_snapshot * t.hours
    # Without a cyclic link the first snapshot's storage row has no previous
    # state of charge, so it loses one term per store.
    cyclic || (nnz -= t.stores)
    return (cols = cols, rows = rows, nnz = nnz)
end

"""
Build the model. Everything inside this function is charged to construction.

The `@constraint` macro with an index set is JuMP's intended construction path
and is what its performance documentation points at. The generator expressions
inside it do not allocate intermediate arrays: the macro lowers them to
in-place accumulation into a single affine expression.
"""
function build_model(t::Topology, backend::Symbol; cyclic::Bool)
    model = if backend === :direct
        m = direct_model(HiGHS.Optimizer())
        set_attribute(m, "output_flag", false)
        m
    else
        Model()
    end
    # JuMP documents this as one of the largest single wins available when
    # building a large model, because every variable and constraint otherwise
    # gets a string name it will never be asked for.
    set_string_names_on_creation(model, false)

    T = t.hours

    @variable(model, 0.0 <= p[1:t.gens, 1:T] <= P_MAX)
    @variable(model, -F_MAX <= f[1:t.lines, 1:T] <= F_MAX)
    @variable(model, -pi <= theta[1:t.buses, 1:T] <= pi)
    @variable(model, shed[1:t.buses, 1:T] >= 0.0)
    @variable(model, 0.0 <= soc[1:t.stores, 1:T] <= SOC_MAX)
    @variable(model, 0.0 <= ch[1:t.stores, 1:T] <= CH_MAX)
    @variable(model, 0.0 <= di[1:t.stores, 1:T] <= CH_MAX)

    # Nodal balance. A line withdraws at bus0 and delivers at bus1, which is
    # the same sign convention linopy_build.py documents.
    @constraint(
        model,
        [b in 1:t.buses, k in 1:T],
        sum(p[g, k] for g in t.gens_at[b]; init = AffExpr(0.0)) -
        sum(f[l, k] for l in t.lines_from[b]; init = AffExpr(0.0)) +
        sum(f[l, k] for l in t.lines_to[b]; init = AffExpr(0.0)) +
        sum(di[s, k] - ch[s, k] for s in t.stores_at[b]; init = AffExpr(0.0)) +
        shed[b, k] == DEMAND
    )

    # DC flow: f = B (theta_from - theta_to).
    @constraint(
        model,
        [l in 1:t.lines, k in 1:T],
        f[l, k] - SUSCEPTANCE * (theta[t.bus0[l], k] - theta[t.bus1[l], k]) == 0.0
    )

    # Storage dynamics across snapshots. Cyclic means the first snapshot's
    # previous state is the last snapshot's, which is what gridwright does.
    if cyclic
        @constraint(
            model,
            [s in 1:t.stores, k in 1:T],
            soc[s, k] - soc[s, k == 1 ? T : k - 1] - EFF * ch[s, k] + di[s, k] / EFF == 0.0
        )
    else
        @constraint(
            model,
            [s in 1:t.stores, k in 2:T],
            soc[s, k] - soc[s, k - 1] - EFF * ch[s, k] + di[s, k] / EFF == 0.0
        )
        @constraint(
            model,
            [s in 1:t.stores],
            soc[s, 1] - EFF * ch[s, 1] + di[s, 1] / EFF == 0.0
        )
    end

    # A linear objective, built with `add_to_expression!` because JuMP's
    # performance guidance is explicit that repeated `+=` on an expression
    # allocates a new one every time. linopy_build.py sets no objective at all,
    # so if anything this charges JuMP for work linopy was never asked to do.
    obj = AffExpr(0.0)
    sizehint!(obj.terms, (t.gens + t.buses) * T)
    for k in 1:T
        for g in 1:t.gens
            add_to_expression!(obj, 12.0 + (g - 1) % 5, p[g, k])
        end
        for b in 1:t.buses
            add_to_expression!(obj, 3000.0, shed[b, k])
        end
    end
    @objective(model, Min, obj)

    return model
end

"""
Nonzeros as the tool itself reports them, which is the number that has to match.

In cached mode this goes through `lp_matrix_data`, which JuMP documents as
pedagogical rather than a solver interface, so its cost is reported separately
and never folded into construction. In direct mode HiGHS already holds the
matrix and can simply be asked.
"""
function measured_nnz(model, backend::Symbol)
    if backend === :direct
        try
            return Int(HiGHS.Highs_getNumNz(JuMP.unsafe_backend(model))), 0.0
        catch
            try
                return Int(HiGHS.Highs_getNumNz(JuMP.unsafe_backend(model).inner)), 0.0
            catch err
                @warn "could not read nonzero count from HiGHS" err
                return -1, 0.0
            end
        end
    end
    t0 = time_ns()
    data = lp_matrix_data(model)
    n = SparseArrays.nnz(data.A)
    return n, (time_ns() - t0) / 1e9
end

function run_once(buses::Int, hours::Int, backend::Symbol, cyclic::Bool, want_matrix::Bool)
    t = topology(buses, hours)

    GC.gc()
    t0 = time_ns()
    model = build_model(t, backend; cyclic = cyclic)
    build_s = (time_ns() - t0) / 1e9

    cols = num_variables(model)
    rows = num_constraints(model; count_variable_in_set_constraints = false)

    nnz, export_s = if want_matrix || backend === :direct
        measured_nnz(model, backend)
    else
        (-1, 0.0)
    end

    model = nothing
    GC.gc()

    return (build_s = build_s, export_s = export_s, cols = cols, rows = rows, nnz = nnz,
            expected = expected_counts(t; cyclic = cyclic))
end

"""
Report a mismatch loudly. A benchmark that quietly builds a smaller problem is
worse than no benchmark, because it reads as a result.
"""
function check_counts(r)
    ok = true
    for (label, got, want) in (("columns", r.cols, r.expected.cols),
                               ("rows", r.rows, r.expected.rows),
                               ("nonzeros", r.nnz, r.expected.nnz))
        if got == -1
            @printf("  %-9s %14s  (not measured in this backend)\n", label, "-")
        elseif got == want
            @printf("  %-9s %14d  matches\n", label, got)
        else
            ok = false
            @printf("  %-9s %14d  MISMATCH, expected %d, difference %+d\n",
                    label, got, want, got - want)
        end
    end
    ok || println("\n  *** COUNTS DO NOT MATCH. The two engines were not asked to " *
                  "build the same problem, and any timing below is not a fair " *
                  "comparison. ***\n")
    return ok
end

function main()
    args = copy(ARGS)
    buses = 256
    hours = 8760
    backend = :cached
    reps = 3
    cyclic = true
    want_matrix = false
    warmup = true

    i = 1
    while i <= length(args)
        a = args[i]
        if a == "--buses"; buses = parse(Int, args[i + 1]); i += 2
        elseif a == "--hours"; hours = parse(Int, args[i + 1]); i += 2
        elseif a == "--reps"; reps = parse(Int, args[i + 1]); i += 2
        elseif a == "--backend"
            b = args[i + 1]
            backend = b == "matrix" ? :cached : Symbol(b)
            want_matrix |= (b == "matrix")
            i += 2
        elseif a == "--no-cyclic"; cyclic = false; i += 1
        elseif a == "--no-warmup"; warmup = false; i += 1
        elseif a == "--matrix"; want_matrix = true; i += 1
        else
            println("unknown argument: ", a)
            exit(2)
        end
    end

    println("JuMP ", pkgversion(JuMP), " on Julia ", VERSION)
    println("backend: ", backend, want_matrix ? " (+ lp_matrix_data export)" : "",
            "   storage: ", cyclic ? "cyclic" : "non-cyclic (matches linopy_build.py)")

    if warmup
        # Force compilation through exactly the code paths and types that the
        # timed runs use. Everything after this measures the program rather
        # than the compiler.
        print("warm-up (8 buses x 24 h, not timed) ... ")
        run_once(8, 24, backend, cyclic, want_matrix)
        println("done")
    end

    println("\nsynthetic network: $buses buses x $hours snapshots, best of $reps")

    best = nothing
    for r in 1:reps
        res = run_once(buses, hours, backend, cyclic, want_matrix)
        @printf("  run %d: construction %8.3f s", r, res.build_s)
        res.export_s > 0 && @printf("   matrix export %8.3f s", res.export_s)
        println()
        if best === nothing || res.build_s < best.build_s
            best = res
        end
    end

    println("\ncounts:")
    check_counts(best)

    println()
    @printf("  CONSTRUCTION: %10.3f s  (best of %d)\n", best.build_s, reps)
    if best.export_s > 0
        # Reported apart from construction because JuMP's own documentation
        # calls lp_matrix_data pedagogical rather than a solver interface.
        @printf("  matrix export:%10.3f s  (lp_matrix_data)\n", best.export_s)
    end
    if best.nnz > 0
        @printf("  throughput:   %10.2f M nonzeros/s\n", best.nnz / best.build_s / 1e6)
    end
end

main()
