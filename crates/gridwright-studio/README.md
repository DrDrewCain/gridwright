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
cargo run -p gridwright-studio -- examples/pglib/case14_ieee.m
cargo run -p gridwright-studio -- path/to/a/pypsa/directory
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
bundled IEEE 14-bus case straight away.

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
corridors brighten with utilisation, with a tick across anything sitting at its
rating. Buses that failed to serve their load are ringed. Hovering a corridor
gives its flow against its rating; clicking a bus fills the inspector with what
is attached and what each machine actually ran at.

**Asking about one hour.** A network with a horizon gets a scrubber. Arrow keys
step, shift-arrow steps a day, and `horizon` shows everything at once. Every
number on the canvas is reduced at the chosen instant.

**Finding things.** `⌘K` (or `ctrl-shift-P`) opens a palette over every bus name
and every command. Going to a bus selects it and brings the camera to it.

**Keys.** `F` fits, `esc` clears the selection, `+` and `-` zoom, `←` and `→`
step the timeline, `,` and `.` jump to its ends.

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
| `backend.rs` | solving, on a thread natively and inline on the web |

`backend.rs` is where the two targets genuinely differ. Native gets a thread;
the browser gets an inline solve with a row-count refusal ahead of it, because a
solve that freezes the tab cannot be cancelled and a refusal that arrives after
the freeze is not a refusal.

## What it does not do yet

No plots — no dispatch stack, no price duration curve, no hour-by-day heatmap,
which the research says is the workhorse chart of this field. No editing. No
scenarios. No accessibility story: `accesskit` is deliberately switched off in
`Cargo.toml` rather than switched on to claim one, because nothing a screen
reader can use exists behind a canvas of painted shapes yet.

The font is still egui's bundled Ubuntu Light, which is wrong for a dense
technical interface and needs a face this repo does not vendor.
