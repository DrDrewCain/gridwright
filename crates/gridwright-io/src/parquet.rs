//! Parquet, for network tables and for the time series that dwarf them.
//!
//! CSV is fine for a network description: a few thousand rows, read once. It
//! is a poor way to move the time series. A year at hourly resolution is 8760
//! snapshots, and a national fleet has thousands of generators, so an
//! availability profile is tens of millions of numbers. As text that is a
//! gigabyte or so, parsed one character at a time. As Parquet it is columnar,
//! typed, compressed, and read as blocks of `f64` that go straight into the
//! buffer they are destined for.
//!
//! The layout mirrors the CSV one exactly — `buses.parquet`, `generators.parquet`
//! and so on in a directory — so a caller converts by writing the same
//! directory in the other format, and a directory may hold both.
//!
//! # Two paths, on purpose
//!
//! Component tables go through [`crate::csv::Table`], the same type the CSV
//! reader produces. They are small, and sharing the type means every format
//! shares one interpretation of an empty cell or a boolean spelling.
//!
//! Time series do not. They are the reason this format is here, and rendering
//! ten million floats to text to parse them back would give up precisely what
//! Parquet was chosen for. Those are read as typed columns straight into the
//! component-major buffer.

use std::path::Path;
use std::sync::Arc;

use arrow_array::{Array, RecordBatch, StringArray, builder::Float64Builder};
use arrow_schema::{DataType, Field, Schema};
use gridwright_net::{Network, TimeSeries};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

use crate::csv::Table;
use crate::{IoError, TableSource};

#[derive(Debug, thiserror::Error)]
pub enum ParquetError {
    #[error("{file}: {source}")]
    Read {
        file: String,
        #[source]
        source: parquet::errors::ParquetError,
    },
    #[error("{file}: column `{column}` holds {found}, which is not a number")]
    NotNumeric {
        file: String,
        column: String,
        found: String,
    },
    #[error("{file}: {got} rows but there are {want} snapshots")]
    Rows {
        file: String,
        got: usize,
        want: usize,
    },
}

/// Render one cell as text, for the tables that go through [`Table`].
///
/// Floats use the shortest representation that reads back exactly, so a value
/// that passes through here is bit-identical on the other side.
fn cell(col: &dyn Array, row: usize) -> String {
    use arrow_array::cast::AsArray;
    use arrow_array::types::*;
    if col.is_null(row) {
        return String::new();
    }
    macro_rules! prim {
        ($t:ty) => {
            return format!("{}", col.as_primitive::<$t>().value(row))
        };
    }
    match col.data_type() {
        DataType::Float64 => return format!("{:?}", col.as_primitive::<Float64Type>().value(row)),
        DataType::Float32 => return format!("{:?}", col.as_primitive::<Float32Type>().value(row)),
        DataType::Int64 => prim!(Int64Type),
        DataType::Int32 => prim!(Int32Type),
        DataType::Int16 => prim!(Int16Type),
        DataType::Int8 => prim!(Int8Type),
        DataType::UInt64 => prim!(UInt64Type),
        DataType::UInt32 => prim!(UInt32Type),
        DataType::UInt16 => prim!(UInt16Type),
        DataType::UInt8 => prim!(UInt8Type),
        DataType::Boolean => return col.as_boolean().value(row).to_string(),
        DataType::Utf8 => return col.as_string::<i32>().value(row).to_string(),
        DataType::LargeUtf8 => return col.as_string::<i64>().value(row).to_string(),
        DataType::Utf8View => return col.as_string_view().value(row).to_string(),
        _ => {}
    }
    // Anything else is rendered through Arrow's own display, which is better
    // than dropping it: a caller reading an unexpected column type at least
    // gets a diagnosable value rather than an empty cell.
    arrow_cast::display::array_value_to_string(col, row).unwrap_or_default()
}

/// Read every row group of a Parquet file into one [`Table`].
fn read_table(path: &Path) -> Result<Option<Table>, IoError> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(IoError::Read {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    table_from(file, label)
}

/// The same, from bytes already in memory.
///
/// `bytes::Bytes` is a chunk reader, so the whole Parquet reader works over a
/// buffer with no filesystem involved.
pub fn table_from_bytes(bytes: Vec<u8>, label: &str) -> Result<Option<Table>, IoError> {
    table_from(bytes::Bytes::from(bytes), label.to_string())
}

fn table_from<R: parquet::file::reader::ChunkReader + 'static>(
    file: R,
    label: String,
) -> Result<Option<Table>, IoError> {
    let fail = |source| {
        IoError::Parquet(ParquetError::Read {
            file: label.clone(),
            source,
        })
    };
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(fail)?
        .build()
        .map_err(fail)?;

    let mut header: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| {
            IoError::Parquet(ParquetError::Read {
                file: label.clone(),
                source: parquet::errors::ParquetError::ArrowError(e.to_string()),
            })
        })?;
        if header.is_empty() {
            header = batch
                .schema()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect();
        }
        for r in 0..batch.num_rows() {
            rows.push(
                (0..batch.num_columns())
                    .map(|c| cell(batch.column(c).as_ref(), r))
                    .collect(),
            );
        }
    }
    Ok(Some(Table::from_parts(header, rows)))
}

/// Read a wide time series as typed columns.
///
/// One column per component, one row per snapshot, transposed into the
/// component-major buffer the engine wants. This is the path that stays
/// numeric end to end.
fn read_wide(
    path: &Path,
    names: &[String],
    n_snapshots: usize,
    default: &[f64],
) -> Result<Option<TimeSeries>, IoError> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(IoError::Read {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    wide_from(file, label, names, n_snapshots, default)
}

/// A wide time series from bytes, staying numeric throughout.
pub fn wide_from_bytes(
    bytes: Vec<u8>,
    label: &str,
    names: &[String],
    n_snapshots: usize,
    default: &[f64],
) -> Result<Option<TimeSeries>, IoError> {
    wide_from(
        bytes::Bytes::from(bytes),
        label.to_string(),
        names,
        n_snapshots,
        default,
    )
}

fn wide_from<R: parquet::file::reader::ChunkReader + 'static>(
    file: R,
    label: String,
    names: &[String],
    n_snapshots: usize,
    default: &[f64],
) -> Result<Option<TimeSeries>, IoError> {
    let fail = |source| {
        IoError::Parquet(ParquetError::Read {
            file: label.clone(),
            source,
        })
    };
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(fail)?
        .build()
        .map_err(fail)?;

    // Component major from the start: component `c` occupies a contiguous run.
    let mut data: Vec<f64> = Vec::new();
    for (c, _) in names.iter().enumerate() {
        let fill = default.get(c).copied().unwrap_or(1.0);
        data.extend(std::iter::repeat_n(fill, n_snapshots));
    }

    let mut column_of: Option<Vec<Option<usize>>> = None;
    let mut at = 0usize;
    for batch in reader {
        let batch = batch.map_err(|e| {
            IoError::Parquet(ParquetError::Read {
                file: label.clone(),
                source: parquet::errors::ParquetError::ArrowError(e.to_string()),
            })
        })?;
        // Columns are matched by component name, so a file listing them in a
        // different order, or listing only some of them, still lands correctly.
        let map = column_of.get_or_insert_with(|| {
            batch
                .schema()
                .fields()
                .iter()
                .map(|f| names.iter().position(|n| n == f.name()))
                .collect()
        });
        for (col, target) in map.iter().enumerate() {
            let Some(component) = *target else { continue };
            let array = batch.column(col);
            for r in 0..batch.num_rows() {
                let t = at + r;
                if t >= n_snapshots {
                    break;
                }
                if array.is_null(r) {
                    continue;
                }
                let v = match array.data_type() {
                    DataType::Float64 => {
                        use arrow_array::cast::AsArray;
                        array.as_primitive::<arrow_array::types::Float64Type>().value(r)
                    }
                    DataType::Float32 => {
                        use arrow_array::cast::AsArray;
                        array.as_primitive::<arrow_array::types::Float32Type>().value(r) as f64
                    }
                    DataType::Int64 => {
                        use arrow_array::cast::AsArray;
                        array.as_primitive::<arrow_array::types::Int64Type>().value(r) as f64
                    }
                    other => {
                        return Err(IoError::Parquet(ParquetError::NotNumeric {
                            file: label,
                            column: batch.schema().field(col).name().clone(),
                            found: other.to_string(),
                        }));
                    }
                };
                data[component * n_snapshots + t] = v;
            }
        }
        at += batch.num_rows();
    }
    if at != n_snapshots {
        return Err(IoError::Parquet(ParquetError::Rows {
            file: label,
            got: at,
            want: n_snapshots,
        }));
    }
    TimeSeries::from_flat(data, names.len(), n_snapshots)
        .map(Some)
        .map_err(IoError::Invalid)
}

/// A directory of Parquet files.
pub struct ParquetDir<'a>(pub &'a Path);

impl TableSource for ParquetDir<'_> {
    fn table(&self, stem: &str) -> Result<Option<Table>, IoError> {
        read_table(&self.0.join(self.label(stem)))
    }

    fn text(&self, name: &str) -> Result<Option<String>, IoError> {
        // The single-value settings stay plain text alongside, since a
        // Parquet file holding one number would be silly.
        let path = self.0.join(name);
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(IoError::Read {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    fn label(&self, stem: &str) -> String {
        format!("{stem}.parquet")
    }
}

/// Load a network from a directory of Parquet files.
///
/// Component tables come through the shared assembler; the two wide time
/// series are then read numerically, replacing whatever the assembler put
/// there. That ordering is deliberate: the assembler needs the component names
/// before a wide file's columns can be matched to components.
pub fn load_network(dir: impl AsRef<Path>) -> Result<Network, IoError> {
    let dir = dir.as_ref();
    let mut net = crate::assemble(&ParquetDir(dir))?;

    let gen_names: Vec<String> = net.generators.iter().map(|g| g.name.clone()).collect();
    let load_names: Vec<String> = net.loads.iter().map(|l| l.name.clone()).collect();
    let n = net.n_snapshots();

    if let Some(ts) = read_wide(
        &dir.join("gen_availability.parquet"),
        &gen_names,
        n,
        &vec![1.0; gen_names.len()],
    )? {
        net.gen_availability = ts;
    }
    let defaults: Vec<f64> = net.loads.iter().map(|l| l.p_set).collect();
    if let Some(ts) = read_wide(&dir.join("load_profile.parquet"), &load_names, n, &defaults)? {
        net.load_profile = ts;
    }
    net.validate()?;
    Ok(net)
}

// --- Writing ---

fn write_batch(path: &Path, batch: RecordBatch) -> Result<(), IoError> {
    let file = std::fs::File::create(path).map_err(|source| IoError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let fail = |source| {
        IoError::Parquet(ParquetError::Read {
            file: label.clone(),
            source,
        })
    };
    // Snappy: pure Rust, so this writer works in a browser, and the default
    // pandas and pyarrow write, so what comes out is what the rest of anyone's
    // toolchain already expects. Zstandard compresses these files better and
    // costs a C toolchain, which is a poor trade for a library whose target
    // includes WebAssembly.
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(file, batch.schema(), Some(props)).map_err(fail)?;
    writer.write(&batch).map_err(fail)?;
    writer.close().map_err(fail)?;
    Ok(())
}

/// Columns as `(name, values)`, built into a batch of `Utf8` and `Float64`.
struct Cols {
    text: Vec<(String, Vec<String>)>,
    real: Vec<(String, Vec<f64>)>,
}

impl Cols {
    fn new() -> Self {
        Self {
            text: Vec::new(),
            real: Vec::new(),
        }
    }
    fn t(&mut self, name: &str, v: Vec<String>) {
        self.text.push((name.into(), v));
    }
    fn r(&mut self, name: &str, v: Vec<f64>) {
        self.real.push((name.into(), v));
    }

    fn batch(self) -> Result<RecordBatch, arrow_schema::ArrowError> {
        let mut fields = Vec::new();
        let mut arrays: Vec<Arc<dyn Array>> = Vec::new();
        for (name, v) in self.text {
            fields.push(Field::new(&name, DataType::Utf8, false));
            arrays.push(Arc::new(StringArray::from(v)));
        }
        for (name, v) in self.real {
            fields.push(Field::new(&name, DataType::Float64, true));
            let mut b = Float64Builder::with_capacity(v.len());
            // Infinity has no Parquet representation either, and a null is the
            // honest encoding: the value is not a number this file can hold.
            // The reader turns an empty cell back into the column's default,
            // which for a capacity ceiling is unbounded.
            for x in v {
                if x.is_finite() {
                    b.append_value(x);
                } else {
                    b.append_null();
                }
            }
            arrays.push(Arc::new(b.finish()));
        }
        RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
    }
}

/// Write a network as a directory of Parquet files.
pub fn write_network(net: &Network, dir: impl AsRef<Path>) -> Result<(), IoError> {
    let dir = dir.as_ref();
    std::fs::create_dir_all(dir).map_err(|source| IoError::Read {
        path: dir.display().to_string(),
        source,
    })?;
    let arrow = |e: arrow_schema::ArrowError| {
        IoError::Parquet(ParquetError::Read {
            file: dir.display().to_string(),
            source: parquet::errors::ParquetError::ArrowError(e.to_string()),
        })
    };

    let mut c = Cols::new();
    c.t("name", net.buses.iter().map(|b| b.name.clone()).collect());
    c.t("country", net.buses.iter().map(|b| b.country.clone()).collect());
    c.t(
        "synchronous_area",
        net.buses.iter().map(|b| b.synchronous_area.clone()).collect(),
    );
    c.t("carrier", net.buses.iter().map(|b| b.carrier.clone()).collect());
    c.r("v_nom", net.buses.iter().map(|b| b.v_nom).collect());
    c.r("g_shunt", net.buses.iter().map(|b| b.g_shunt).collect());
    c.r("b_shunt", net.buses.iter().map(|b| b.b_shunt).collect());
    c.r("v_min", net.buses.iter().map(|b| b.v_min).collect());
    c.r("v_max", net.buses.iter().map(|b| b.v_max).collect());
    write_batch(&dir.join("buses.parquet"), c.batch().map_err(arrow)?)?;

    let bus = |i: usize| net.buses[i].name.clone();
    let mut c = Cols::new();
    c.t("name", net.generators.iter().map(|g| g.name.clone()).collect());
    c.t("bus", net.generators.iter().map(|g| bus(g.bus)).collect());
    c.t("carrier", net.generators.iter().map(|g| g.carrier.clone()).collect());
    c.t(
        "p_nom_extendable",
        net.generators
            .iter()
            .map(|g| g.p_nom_extendable.to_string())
            .collect(),
    );
    c.t(
        "committable",
        net.generators.iter().map(|g| g.committable.to_string()).collect(),
    );
    c.r("p_nom", net.generators.iter().map(|g| g.p_nom).collect());
    c.r("p_nom_max", net.generators.iter().map(|g| g.p_nom_max).collect());
    c.r("p_min_pu", net.generators.iter().map(|g| g.p_min_pu).collect());
    c.r(
        "marginal_cost",
        net.generators.iter().map(|g| g.marginal_cost).collect(),
    );
    c.r(
        "capital_cost",
        net.generators.iter().map(|g| g.capital_cost).collect(),
    );
    c.r(
        "co2_emissions",
        net.generators.iter().map(|g| g.co2_emissions).collect(),
    );
    c.r(
        "embodied_co2",
        net.generators.iter().map(|g| g.embodied_co2).collect(),
    );
    c.r("ramp_up", net.generators.iter().map(|g| g.ramp_up).collect());
    c.r("ramp_down", net.generators.iter().map(|g| g.ramp_down).collect());
    c.r("q_min", net.generators.iter().map(|g| g.q_min).collect());
    c.r("q_max", net.generators.iter().map(|g| g.q_max).collect());
    write_batch(&dir.join("generators.parquet"), c.batch().map_err(arrow)?)?;

    let mut c = Cols::new();
    c.t("name", net.lines.iter().map(|l| l.name.clone()).collect());
    c.t("bus0", net.lines.iter().map(|l| bus(l.bus0)).collect());
    c.t("bus1", net.lines.iter().map(|l| bus(l.bus1)).collect());
    c.r("s_nom", net.lines.iter().map(|l| l.s_nom).collect());
    c.r("susceptance", net.lines.iter().map(|l| l.susceptance).collect());
    c.r("resistance", net.lines.iter().map(|l| l.resistance).collect());
    c.r("reactance", net.lines.iter().map(|l| l.reactance).collect());
    c.r(
        "shunt_susceptance",
        net.lines.iter().map(|l| l.shunt_susceptance).collect(),
    );
    c.r("tap_ratio", net.lines.iter().map(|l| l.tap_ratio).collect());
    c.r("phase_shift", net.lines.iter().map(|l| l.phase_shift).collect());
    c.r("loss", net.lines.iter().map(|l| l.loss).collect());
    write_batch(&dir.join("lines.parquet"), c.batch().map_err(arrow)?)?;

    let mut c = Cols::new();
    c.t("name", net.loads.iter().map(|l| l.name.clone()).collect());
    c.t("bus", net.loads.iter().map(|l| bus(l.bus)).collect());
    c.r("p_set", net.loads.iter().map(|l| l.p_set).collect());
    c.r("q_set", net.loads.iter().map(|l| l.q_set).collect());
    write_batch(&dir.join("loads.parquet"), c.batch().map_err(arrow)?)?;

    if !net.storage.is_empty() {
        let mut c = Cols::new();
        c.t("name", net.storage.iter().map(|s| s.name.clone()).collect());
        c.t("bus", net.storage.iter().map(|s| bus(s.bus)).collect());
        c.t(
            "cyclic",
            net.storage.iter().map(|s| s.cyclic.to_string()).collect(),
        );
        c.r("p_nom", net.storage.iter().map(|s| s.p_nom).collect());
        c.r("max_hours", net.storage.iter().map(|s| s.max_hours).collect());
        c.r(
            "efficiency_store",
            net.storage.iter().map(|s| s.efficiency_store).collect(),
        );
        c.r(
            "efficiency_dispatch",
            net.storage.iter().map(|s| s.efficiency_dispatch).collect(),
        );
        write_batch(&dir.join("storage_units.parquet"), c.batch().map_err(arrow)?)?;
    }

    let mut c = Cols::new();
    c.r("weight", net.snapshots.weights().to_vec());
    write_batch(&dir.join("snapshots.parquet"), c.batch().map_err(arrow)?)?;

    // Wide series, one column per component.
    let n = net.n_snapshots();
    let wide = |names: &[String], ts: &TimeSeries| -> Option<Cols> {
        if ts.is_empty() {
            return None;
        }
        let mut c = Cols::new();
        for (i, name) in names.iter().enumerate() {
            c.r(name, ts.row(i).map(<[f64]>::to_vec).unwrap_or(vec![0.0; n]));
        }
        Some(c)
    };
    let gen_names: Vec<String> = net.generators.iter().map(|g| g.name.clone()).collect();
    if let Some(c) = wide(&gen_names, &net.gen_availability) {
        write_batch(
            &dir.join("gen_availability.parquet"),
            c.batch().map_err(arrow)?,
        )?;
    }
    let load_names: Vec<String> = net.loads.iter().map(|l| l.name.clone()).collect();
    if let Some(c) = wide(&load_names, &net.load_profile) {
        write_batch(&dir.join("load_profile.parquet"), c.batch().map_err(arrow)?)?;
    }
    Ok(())
}
