# gridwright studio

The interactive shell: a network view and a solve, in a native window or a
browser tab, running the same code in both.

The browser build is the point. It has no server — the model is built and solved
in the tab, on the reader's own machine, which is why the solver in
`gridwright-simplex` exists at all. Every mature LP library is C or C++ and does
not cross to `wasm32-unknown-unknown`.

## Running it

Native, with a file or a directory:

```sh
cargo run -p gridwright-studio -- examples/demo-grid
cargo run -p gridwright-studio -- examples/pglib/case14_ieee.m
```

A PyPSA network *is* a directory — `buses.csv`, `lines.csv`, and one file per
time series — so a path to one works as well as a path to a single file. The
browser cannot do this: it has no filesystem, and everything reaches it as
bytes.

In a browser:

```sh
./crates/gridwright-studio/build-web.sh
python3 -m http.server 8777 --directory crates/gridwright-studio
```

then open `http://localhost:8777/index.html`. The `#demo` fragment loads the
bundled demo straight away.

That demo is `examples/demo-grid`: eight substations across 380, 220 and 110 kV
on a north-south German corridor, at real coordinates, with offshore wind in the
north, gas in the south, a solar farm, a pumped-storage unit, and twenty-four
hourly snapshots. The corridor between north and south is deliberately tight, so
it binds and the nodal prices separate.

It is deliberately not IEEE 14-bus, which is the right *test* case and the wrong
demo: MATPOWER carries no coordinates, its `baseKV` is 1.0 throughout because
the case is written in per unit, it has one snapshot, and it has no congestion —
so opening it exercised none of the geographic layout, none of the voltage
colouring, none of the timeline and none of the price ramp.

The bundle is about 7 MB on disk and 2 MB compressed. The second number is the
one that crosses the wire; any static host worth deploying to serves it
precompressed.

## What the interface does

**Reading the picture.** Buses are drawn as busbars rather than dots, because a
busbar has length — more than one thing connects to it — and circuits tap onto
it perpendicularly at points spread along the bar, ordered so nothing crosses on
the approach. Generators, loads and storage carry their IEC symbols. Corridor
width is the square root of rating; corridor colour is voltage where the file
states it, and kind where it does not.

**Reading the answer.** Once solved, busbars brighten with nodal price and
corridors brighten with utilisation, with a chevron for direction and a tick
across anything sitting at its rating. Buses that failed to serve their load are
ringed. Generators take their carrier's colour, so where the wind is and where
the gas is reads at a glance.

**Charts.** Clicking a bus gives its price over the horizon, the duration curve
of that price, what is attached, what each machine ran at, the energy balance,
and — where there is storage — its state of charge against capacity. Clicking a
corridor gives its flow against its rating in both directions. The results panel
carries the system dispatch stack, coloured by carrier and ordered by merit.

**Dragging any chart scrubs the timeline**, so pointing at a spike shows the
network at that hour.

**Asking about one hour.** A network with a horizon gets a scrubber. Arrow keys
step, shift-arrow steps a day, and `horizon` shows everything at once. Every
number on the canvas is reduced at the chosen instant.

**Finding things.** `⌘K` (or `ctrl-shift-P`) opens a palette over every bus,
every corridor and every command, including "go to the most congested hour" and
"go to the worst hour" — a year has 8,760 hours and finding the interesting
three by dragging is a search the tool should do. Going to something selects it
and brings the camera to it.

**Keys.** `F` fits, `esc` clears the selection, `+` and `-` zoom, `←` and `→`
step the timeline, `,` and `.` jump to its ends.

## The basemap

Coastlines and borders draw under the network **when, and only when, the layout
is geographic**. Under a spring embedding they would place substations on a map
they have no relationship to, which is a worse lie than no map — the whole point
of the origin label in the status strip is that the two pictures are otherwise
indistinguishable.

Three layers: land filled a shade lighter than the sea, lakes painted back out
in the sea tone, and national borders as a separate thinner hairline. A tonal
land/sea distinction does more for orientation than any amount of outline
detail, which is why every published TSO map has one.

Natural Earth 1:50m, public domain, simplified to about 11 km, quantised to
`i16` and **triangulated ahead of time** — ear clipping a coastline is O(n²) in
the worst case and belongs in a build step, so the runtime is a decode and a
transform with no geometry algorithm in it. 143 KB, about 2% of the bundle.

**Bundled rather than fetched** — a tile layer would give a serverless tool a
server, plus somebody else's terms of service and a network round trip. This
works on a plane.

And no more than three layers. Overbye (NAPS 2019) on geographic grid displays:
satellite and detailed backgrounds *"run the risk of background camouflaging the
electric grid information of interest."* No roads, no terrain, no labels, and
the whole thing sits within a few percent of the canvas tone.

## Layout

Positions come from geography when the file carries it — PyPSA does, in columns
it calls `x` and `y` — projected with Web Mercator. Otherwise they are invented
from the topology with a spring embedder. A partly-located file gets both: the
placed buses are pinned and the rest arranged around them.

The status strip says which, because the picture cannot. A projection and a
relaxation are both marks joined by lines and look equally authoritative, and a
reader who assumes the wrong one will draw conclusions about distance that the
picture does not support.

## Structure

| file | what |
| --- | --- |
| `app.rs` | the shell: panels, timeline, results, reductions |
| `view.rs` | the canvas: camera, symbols, corridors, picking |
| `layout.rs` | where a bus goes — projection or relaxation |
| `theme.rs` | the visual language, in one place |
| `palette.rs` | go-to and commands |
| `fuzzy.rs` | subsequence matching for the palette |
| `chart.rs` | small charts: line, band, stack, duration, threshold |
| `basemap.rs` | bundled coastlines, drawn under a geographic layout |
| `backend.rs` | solving, on a thread natively and inline on the web |

`backend.rs` is where the two targets genuinely differ. Native gets a thread;
the browser gets an inline solve with a row-count refusal ahead of it, because a
solve that freezes the tab cannot be cancelled and a refusal that arrives after
the freeze is not a refusal.

## What it does not do yet

No hour-by-day heatmap, which the research says is the workhorse chart of this
field — the charts here are time series, duration curves and one stack. No
editing. No scenarios. No network diff. No infeasibility diagnosis beyond
showing where load went unserved. No accessibility story: `accesskit` is
deliberately switched off in `Cargo.toml` rather than switched on to claim one,
because nothing a screen reader can use exists behind a canvas of painted shapes
yet.

The font is still egui's bundled Ubuntu Light, which is wrong for a dense
technical interface and needs a face this repo does not vendor.
