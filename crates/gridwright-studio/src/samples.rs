//! The networks the studio carries with it.
//!
//! **Embedded, not fetched.** A browser tab has no working directory, and the
//! claim this project makes is that it runs with no server behind it — so a
//! reader who wants to try the thing has to be able to, on the first paint, from
//! a `file://` URL, on a plane. Every case here is compiled in.
//!
//! # Why more than one
//!
//! One sample teaches one shape of problem. The IEEE cases differ in ways that
//! matter to a formulation rather than only in size: the Reliability Test System
//! puts a dozen generating units on a single bus, the DTC case has twelve
//! generators for a hundred and thirteen loads, and PJM's five-bus case exists
//! precisely because it separates nodal prices where a smaller one cannot. A tool
//! that only ever sees the 14-bus case is a tool whose bugs live in the other
//! twelve.
//!
//! # What they cannot show
//!
//! **None of the IEEE cases carry coordinates.** They are bus numbers and branch
//! impedances, so the studio arranges them by relaxing the topology and says so in
//! the status strip. The basemap and the place names stay off, deliberately: a
//! coastline under invented positions is a map of somewhere that does not exist.
//! `demo-grid` is the one case here with real positions, a carrier per generator
//! and a day of hourly data, which is why it is the default.

/// One network the studio can open without touching a filesystem.
pub struct Sample {
    /// What the reader sees as the network's name, and what the reader would
    /// search for. The file name, because that is the thing that exists on disk.
    pub name: &'static str,
    /// How it is offered in a list. The file name is unreadable as a label and
    /// the label is unfindable as a file, so both are kept.
    pub label: &'static str,
    /// What this case exercises that the others do not. Not a size, which the
    /// bus count already says.
    pub note: &'static str,
    pub buses: usize,
    /// Whether the buses came with positions. Decides whether opening it shows a
    /// map at all, so it belongs in the list rather than being a surprise.
    pub located: bool,
    pub bytes: &'static [u8],
}

/// Every embedded case, smallest first.
///
/// Smallest first because the list is also the reading order: someone finding out
/// what this does should meet the three-bus case before the three-hundred-bus one.
/// `demo-grid` leads regardless of size, since it is the only one that exercises
/// the interface rather than only the solver.
pub const ALL: &[Sample] = &[
    Sample {
        name: "demo-grid.json",
        label: "demo grid — north to south Germany",
        note: "real positions, fuels, storage and a day of hourly data",
        buses: 8,
        located: true,
        bytes: include_bytes!("../../../examples/demo-grid.json"),
    },
    Sample {
        name: "case3_lmbd.m",
        label: "3-bus (Lesieutre, Molzahn, Borden, DeMarco)",
        note: "the smallest network with a loop in it",
        buses: 3,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case3_lmbd.m"),
    },
    Sample {
        name: "case5_pjm.m",
        label: "PJM 5-bus",
        note: "the standard example of congestion separating nodal prices",
        buses: 5,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case5_pjm.m"),
    },
    Sample {
        name: "case14_ieee.m",
        label: "IEEE 14-bus",
        note: "the 1962 AEP case, and the field's default example",
        buses: 14,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case14_ieee.m"),
    },
    Sample {
        name: "case24_ieee_rts.m",
        label: "IEEE 24-bus RTS",
        note: "the Reliability Test System: 33 units on 24 buses",
        buses: 24,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case24_ieee_rts.m"),
    },
    Sample {
        name: "case30_as.m",
        label: "30-bus (Alsac and Stott)",
        note: "the security-constrained variant of the 30-bus case",
        buses: 30,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case30_as.m"),
    },
    Sample {
        name: "case30_ieee.m",
        label: "IEEE 30-bus",
        note: "six generators against twenty-one loads",
        buses: 30,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case30_ieee.m"),
    },
    Sample {
        name: "case39_epri.m",
        label: "39-bus New England (EPRI)",
        note: "the reference case for transient stability work",
        buses: 39,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case39_epri.m"),
    },
    Sample {
        name: "case57_ieee.m",
        label: "IEEE 57-bus",
        note: "seven generators against forty-two loads",
        buses: 57,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case57_ieee.m"),
    },
    Sample {
        name: "case73_ieee_rts.m",
        label: "IEEE 73-bus RTS",
        note: "three Reliability Test System areas tied together, 99 units",
        buses: 73,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case73_ieee_rts.m"),
    },
    Sample {
        name: "case118_ieee.m",
        label: "IEEE 118-bus",
        note: "the largest case a meshed diagram still reads at",
        buses: 118,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case118_ieee.m"),
    },
    Sample {
        name: "case162_ieee_dtc.m",
        label: "IEEE 162-bus DTC",
        note: "twelve generators for a hundred and thirteen loads",
        buses: 162,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case162_ieee_dtc.m"),
    },
    Sample {
        name: "case300_ieee.m",
        label: "IEEE 300-bus",
        note: "the largest of the IEEE cases",
        buses: 300,
        located: false,
        bytes: include_bytes!("../../../examples/pglib/case300_ieee.m"),
    },
];

/// The one opened on the first paint.
///
/// `demo-grid`, because it is the only case that exercises the interface. Opening
/// on an IEEE case shows a relaxed topology with one snapshot, no fuels and no
/// congestion, so the timeline, the fuel key, the price ramp and the map are all
/// inert — which reads as an application with nothing in it. That was the state
/// this shipped in once.
pub const DEFAULT: usize = 0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sample_reads() {
        // Embedded bytes that do not parse are a button that shows an error, and
        // nothing else in the suite would open them.
        for s in ALL {
            gridwright_worker::load(Some(s.name), s.bytes)
                .unwrap_or_else(|e| panic!("{}: {}: {}", s.name, e.kind, e.message));
        }
    }

    #[test]
    fn the_bus_counts_in_the_list_are_the_real_ones() {
        // **The list is shown to the reader, so it is a claim.** A hand-written
        // count drifts the moment a file is replaced, and a list that says 118
        // next to a case with 300 buses in it is worse than a list with no counts.
        for s in ALL {
            let net = gridwright_worker::load(Some(s.name), s.bytes).unwrap().network;
            assert_eq!(net.buses.len(), s.buses, "{} claims {} buses", s.name, s.buses);
        }
    }

    #[test]
    fn the_list_says_which_cases_have_positions() {
        // Also a claim, and the one that decides whether a map appears. The IEEE
        // cases are bus numbers and impedances; only the demo grid is a projection
        // of anywhere.
        for s in ALL {
            let net = gridwright_worker::load(Some(s.name), s.bytes).unwrap().network;
            let origin = crate::layout::layout(&net).kind;
            let geographic = origin == crate::layout::Origin::Geographic;
            assert_eq!(
                geographic, s.located,
                "{} claims located = {}, but its layout is {origin:?}",
                s.name, s.located,
            );
        }
    }

    #[test]
    fn names_and_labels_are_distinct() {
        // Two entries with one name is a list where picking one opens the other.
        for (i, a) in ALL.iter().enumerate() {
            for b in &ALL[i + 1..] {
                assert_ne!(a.name, b.name, "{} appears twice", a.name);
                assert_ne!(a.label, b.label, "{:?} appears twice", a.label);
            }
        }
    }

    #[test]
    fn the_default_is_the_one_with_a_map_and_a_timeline() {
        // Opening on a case with one snapshot and no coordinates leaves the
        // timeline, the fuel key, the price ramp and the basemap all inert, which
        // reads as an application with nothing in it.
        let d = &ALL[DEFAULT];
        assert!(d.located, "the default case has no positions");
        let net = gridwright_worker::load(Some(d.name), d.bytes).unwrap().network;
        assert!(net.snapshots.len() > 1, "the default case has one snapshot");
        assert!(
            net.generators.iter().any(|g| !g.carrier.is_empty()),
            "the default case names no fuels",
        );
    }

    #[test]
    fn the_ieee_family_is_all_here() {
        // PGLib-OPF v23.07 carries twelve cases from the IEEE and adjacent
        // reference families. If one is missing, the list is quietly incomplete.
        for want in [
            "case3_lmbd.m",
            "case5_pjm.m",
            "case14_ieee.m",
            "case24_ieee_rts.m",
            "case30_as.m",
            "case30_ieee.m",
            "case39_epri.m",
            "case57_ieee.m",
            "case73_ieee_rts.m",
            "case118_ieee.m",
            "case162_ieee_dtc.m",
            "case300_ieee.m",
        ] {
            assert!(ALL.iter().any(|s| s.name == want), "{want} is not offered");
        }
    }

    #[test]
    fn every_case_carries_a_note_about_what_it_is_for() {
        // A list of names and sizes does not help anyone choose. The note is the
        // only part that says why a reader would open this one.
        for s in ALL {
            assert!(!s.note.is_empty(), "{} has no note", s.name);
            assert!(!s.label.is_empty(), "{} has no label", s.name);
        }
    }
}
