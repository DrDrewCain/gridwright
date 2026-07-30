# `eu-grid.json` — where every number in it comes from

A pan-European transmission network with real positions and real substation
names, built by `cargo run -p gridwright-mapgen --bin netgen --release`. It is
committed so an ordinary build never rebuilds it.

**Read the second table before using this for anything.** The file is a join of
three published sources and two sets of assumptions that are this project's, and
they are not distinguishable by looking at the result.

## Published, and taken as given

| part | source | licence |
| --- | --- | --- |
| 7,893 buses with lon/lat, names, voltages | GridKit extract of the ENTSO-E interactive map, `10.5281/zenodo.55853` | CC BY 4.0 |
| 9,784 lines: voltage, circuits, length, DC flag | the same extract | CC BY 4.0 |
| 1,060 transformers coupling voltage levels | the same extract | CC BY 4.0 |
| 1,500 generator sites with fuel and capacity | the same extract | CC BY 4.0 |
| mean hourly demand for 31 countries | Open Power System Data, *Time series* 2020-10-06, `10.25832/time_series/2020-10-06`, primary data from ENTSO-E Transparency | CC BY |

The GridKit dataset is an **unofficial** extract of a map published in **May
2016**. It is neither approved nor endorsed by ENTSO-E, and it is a snapshot of a
map rather than of a grid: it is incomplete, and where it is incomplete this
network is too.

## Ours, and therefore not evidence about Europe

| part | what was assumed |
| --- | --- |
| which bus each country's demand sits on | Each populated place in Natural Earth is split across its five nearest substations by inverse distance, and each country's published mean demand is divided among its own substations in proportion. Nobody publishes per-bus demand for Europe; national totals and substation positions are published, and getting from one to the other is a modelling step however it is done. |
| cost per MWh by fuel | A merit order — wind and solar near zero, then hydro, nuclear, lignite, coal, gas, oil. The ordering is uncontroversial and is what a dispatch needs; the magnitudes are round numbers in the right region for European wholesale markets and are not a claim about anybody's costs. |
| line thermal ratings | Estimated per circuit from voltage. The extract carries no ratings, and these decide where congestion appears, so they are the assumption most worth knowing about. |
| line susceptances | Estimated from voltage and length. The extract carries geometry, not impedance. |
| synchronous areas | All buses are placed in one. The extract spans the European synchronous area plus North Africa and part of the Middle East and does not record which of those are asynchronous, so declaring areas from it would be inventing them. |

## What the result does, and does not, show

Solved as a DC optimal power flow it reaches `Optimal` and sheds roughly 13% of
demand. **That figure measures the assumptions above, not Europe.** Two things
cause it, and both are properties of the source rather than of the grid:

- The extract's topology is incomplete, so 139 electrical islands remain after
  the transformers are included. The largest holds 7,087 of the 7,893 buses; the
  rest are stubs and fragments, and about 4,500 MW of demand sits in islands with
  too little capacity to serve it.
- The estimated line ratings constrain delivery into the buses the demand
  allocation loaded most heavily.

Twenty-nine countries in the extract have no published demand series here, so
their buses carry no load at all.

This file is useful for what it is: a real, named, geolocated transmission
topology at continental scale. It is not a study of European adequacy, and no
such claim is made anywhere in this repository.
