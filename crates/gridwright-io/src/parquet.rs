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
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::{ArrowWriter, ProjectionMask};
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use parquet::file::reader::{ChunkReader, Length};

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

/// One source of bytes, shared by the several readers a wide file needs.
///
/// A [`ChunkReader`] is consumed by the builder, and a `File` cannot be cloned
/// cheaply, so reading a file in slices of columns needs something that can be.
/// This is that and nothing else: every method forwards. The reads are
/// positional, so the copies do not interfere.
struct Shared<R>(Arc<R>);

impl<R> Clone for Shared<R> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<R: Length> Length for Shared<R> {
    fn len(&self) -> u64 {
        self.0.len()
    }
}

impl<R: ChunkReader> ChunkReader for Shared<R> {
    type T = R::T;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        self.0.get_read(start)
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<bytes::Bytes> {
        self.0.get_bytes(start, length)
    }
}

/// How many components are read at a time.
///
/// The reader holds one decoded page per column it is decoding, so decoding
/// every column at once holds a page per component. Where a row group spans the
/// whole horizon — which it does for anything written in one pass, this crate's
/// own writer included — that page *is* the component's entire year, and
/// walking the file in row batches bounds nothing: the batch is small and the
/// pages behind it are the file. Projecting a slice of the columns instead is
/// what bounds it, because a column outside the projection is never decoded.
///
/// Measured on a 4,000 generator by 8,760 snapshot file, whose answer is
/// 280 MB, peak resident memory against this number is 305 MB at 32, 315 at 64,
/// 329 at 128, 346 at 256 and 760 at 4,000, which is every column at once and
/// therefore what this replaces. Wall clock is 0.39 to 0.41 s throughout and
/// 0.47 to 0.61 s for all at once, so nothing is being traded: reading a slice
/// at a time is also faster, because a slice's share of the destination fits in
/// cache where the whole of it does not.
///
/// 128 is the corner. Below it the saving is a few megabytes a step and the
/// number of passes over the schema keeps doubling; above it the cost grows
/// without buying anything. A page is capped at a megabyte by the writer
/// whatever the horizon, so this bounds the reader's own memory at 128 MB in
/// the worst case and at 9 MB for a year of hourly data.
const COLUMNS_AT_ONCE: usize = 128;

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
    // The footer is parsed once and handed to every reader built below, since
    // a wide file's schema is one entry per component and re-reading it for
    // each slice of columns would be the one part of this that does scale with
    // the file.
    let source = Shared(Arc::new(file));
    let meta = ArrowReaderMetadata::load(&source, ArrowReaderOptions::new()).map_err(fail)?;
    let n_columns = meta.metadata().file_metadata().schema_descr().num_columns();

    // Component major from the start: component `c` occupies a contiguous run.
    //
    // Allocated once, at exactly its final size, which is known before a single
    // value has been read. This was a `Vec::new()` grown a component at a time,
    // which doubles as it goes and reaches a moment holding the old 143 MB
    // buffer and the new 287 MB one at once for a 280 MB answer.
    //
    // That moment measures as nothing: peak resident memory is 318 to 328 MB
    // either way. The allocator grows a block this size in place rather than
    // copying it, so the doubling never becomes two resident copies, and this
    // metric would not see it if it did. Kept regardless — it is four thousand
    // reallocations that need not happen, it leaves the answer holding 6.7 MB
    // of capacity it will never use, and address space is not free where there
    // is 4 GB of it in total.
    let mut data: Vec<f64> = Vec::with_capacity(names.len() * n_snapshots);
    for (c, _) in names.iter().enumerate() {
        let fill = default.get(c).copied().unwrap_or(1.0);
        data.extend(std::iter::repeat_n(fill, n_snapshots));
    }

    // Rows seen, which every slice of columns agrees on because they all cover
    // the whole file. Checked at the end rather than after the first slice, so
    // that a file which is both short and badly typed reports the same thing it
    // used to.
    let mut rows = 0usize;
    for first in (0..n_columns).step_by(COLUMNS_AT_ONCE) {
        let last = (first + COLUMNS_AT_ONCE).min(n_columns);
        let reader =
            ParquetRecordBatchReaderBuilder::new_with_metadata(source.clone(), meta.clone())
                .with_projection(ProjectionMask::leaves(
                    meta.metadata().file_metadata().schema_descr(),
                    first..last,
                ))
                .build()
                .map_err(fail)?;

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
            // A projected batch holds only the slice's columns, so the mapping is
            // rebuilt per slice — by name, which makes it independent of where the
            // slice starts.
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
                            array
                                .as_primitive::<arrow_array::types::Float64Type>()
                                .value(r)
                        }
                        DataType::Float32 => {
                            use arrow_array::cast::AsArray;
                            array
                                .as_primitive::<arrow_array::types::Float32Type>()
                                .value(r) as f64
                        }
                        DataType::Int64 => {
                            use arrow_array::cast::AsArray;
                            array
                                .as_primitive::<arrow_array::types::Int64Type>()
                                .value(r) as f64
                        }
                        other => {
                            return Err(IoError::Parquet(ParquetError::NotNumeric {
                                file: label.clone(),
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
        rows = at;
    }
    if rows != n_snapshots {
        return Err(IoError::Parquet(ParquetError::Rows {
            file: label,
            got: rows,
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

    fn wide(
        &self,
        _stem: &str,
        _names: &[String],
        _n: usize,
        _defaults: &[f64],
        _kind: &'static str,
    ) -> Result<Option<gridwright_net::TimeSeries>, IoError> {
        // Read separately, and numerically, once the component names are known.
        // Going through the shared path would render every value to text and
        // parse it back, which is precisely what this format exists to avoid.
        Ok(None)
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
