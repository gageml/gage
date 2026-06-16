use std::io::Write;
use std::str::FromStr;

use arrow::array::{
    Array, BooleanArray, Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array,
    LargeStringArray, StringArray, StringViewArray, UInt8Array, UInt16Array, UInt32Array,
    UInt64Array,
};
use arrow::csv::writer::WriterBuilder;
use arrow::datatypes::DataType;
use arrow::json::{ArrayWriter, LineDelimitedWriter};
use arrow::record_batch::RecordBatch;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use arrow::util::pretty::pretty_format_batches;
use datafusion::error::{DataFusionError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PrintFormat {
    Table,
    Csv,
    Json,
    #[value(name = "ndjson")]
    NdJson,
    Yaml,
}

impl FromStr for PrintFormat {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        clap::ValueEnum::from_str(s, true)
    }
}

impl PrintFormat {
    /// Print one batch incrementally. `is_first` indicates whether any
    /// preceding non-empty batch has been emitted in this run — used by
    /// formats that condition headers/separators on it (Csv writes the
    /// header only on the first batch; Yaml omits the leading `---`).
    /// Empty batches are skipped.
    pub fn print_batch(&self, batch: &RecordBatch, is_first: bool) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        match self {
            Self::Table => {
                let formatted = pretty_format_batches(std::slice::from_ref(batch))?;
                println!("{formatted}");
            }
            Self::Csv => {
                let mut writer = WriterBuilder::new()
                    .with_header(is_first)
                    .build(std::io::stdout());
                writer.write(batch)?;
            }
            Self::Json => {
                // Json array form: emit one self-contained array per
                // batch. A single array spanning every batch can't be
                // produced without buffering — use ndjson for clean
                // streaming.
                let mut buf: Vec<u8> = Vec::new();
                let mut writer = ArrayWriter::new(&mut buf);
                writer.write(batch)?;
                writer.finish()?;
                println!("{}", String::from_utf8_lossy(&buf));
            }
            Self::NdJson => {
                let mut writer = LineDelimitedWriter::new(std::io::stdout());
                writer.write(batch)?;
                writer.finish()?;
            }
            Self::Yaml => {
                let stdout = std::io::stdout();
                let mut out = std::io::BufWriter::new(stdout.lock());
                write_yaml_batch(&mut out, batch, is_first)?;
                out.flush()
                    .map_err(|e| DataFusionError::Execution(format!("yaml stdout flush: {e}")))?;
            }
        }
        Ok(())
    }

    /// Convenience for non-streaming callers — loops over `print_batch`
    /// tracking the `is_first` flag.
    pub fn print_batches(&self, batches: &[RecordBatch]) -> Result<()> {
        let mut is_first = true;
        for batch in batches {
            if batch.num_rows() == 0 {
                continue;
            }
            self.print_batch(batch, is_first)?;
            is_first = false;
        }
        Ok(())
    }
}

/// Emit `batches` as multi-document YAML — one document per row,
/// `---` separator between rows (not before the first). Field order
/// follows the schema. Cell values are typed where possible
/// (utf8/int/float/bool/null); anything else falls back to its Arrow
/// display string. Escaping is handled by `serde_yaml`.
pub fn write_yaml<W: Write>(w: &mut W, batches: &[RecordBatch]) -> Result<()> {
    let mut is_first = true;
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        write_yaml_batch(w, batch, is_first)?;
        is_first = false;
    }
    Ok(())
}

/// Emit one batch in the format described by `write_yaml`. The first
/// row of the run gets no leading separator; every subsequent row
/// (including the first row of a non-first batch) is preceded by `---`.
pub fn write_yaml_batch<W: Write>(
    w: &mut W,
    batch: &RecordBatch,
    is_first_batch: bool,
) -> Result<()> {
    let format_opts = FormatOptions::default();
    let formatters: Vec<ArrayFormatter> = batch
        .columns()
        .iter()
        .map(|c| ArrayFormatter::try_new(c.as_ref(), &format_opts))
        .collect::<Result<_, _>>()?;
    let schema = batch.schema();
    for row in 0..batch.num_rows() {
        if !(is_first_batch && row == 0) {
            writeln!(w, "---")
                .map_err(|e| DataFusionError::Execution(format!("yaml write: {e}")))?;
        }
        let mut mapping = serde_yaml::Mapping::with_capacity(schema.fields().len());
        for (col_idx, field) in schema.fields().iter().enumerate() {
            let col = batch.column(col_idx);
            let fmt = formatters
                .get(col_idx)
                .expect("formatters correspond to batch schema fields");
            let value = cell_to_yaml(col.as_ref(), row, fmt);
            mapping.insert(serde_yaml::Value::String(field.name().clone()), value);
        }
        serde_yaml::to_writer(&mut *w, &serde_yaml::Value::Mapping(mapping))
            .map_err(|e| DataFusionError::Execution(format!("yaml encode: {e}")))?;
    }
    Ok(())
}

fn cell_to_yaml(col: &dyn Array, row: usize, fallback: &ArrayFormatter) -> serde_yaml::Value {
    use serde_yaml::Value;
    if col.is_null(row) {
        return Value::Null;
    }
    match col.data_type() {
        DataType::Utf8 => Value::String(
            col.as_any()
                .downcast_ref::<StringArray>()
                .expect("Utf8 → StringArray")
                .value(row)
                .to_string(),
        ),
        DataType::LargeUtf8 => Value::String(
            col.as_any()
                .downcast_ref::<LargeStringArray>()
                .expect("LargeUtf8 → LargeStringArray")
                .value(row)
                .to_string(),
        ),
        DataType::Utf8View => Value::String(
            col.as_any()
                .downcast_ref::<StringViewArray>()
                .expect("Utf8View → StringViewArray")
                .value(row)
                .to_string(),
        ),
        DataType::Boolean => Value::Bool(
            col.as_any()
                .downcast_ref::<BooleanArray>()
                .expect("Boolean → BooleanArray")
                .value(row),
        ),
        DataType::Int8 => {
            i64_value(col.as_any().downcast_ref::<Int8Array>().unwrap().value(row) as i64)
        }
        DataType::Int16 => i64_value(
            col.as_any()
                .downcast_ref::<Int16Array>()
                .unwrap()
                .value(row) as i64,
        ),
        DataType::Int32 => i64_value(
            col.as_any()
                .downcast_ref::<Int32Array>()
                .unwrap()
                .value(row) as i64,
        ),
        DataType::Int64 => i64_value(
            col.as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(row),
        ),
        DataType::UInt8 => u64_value(
            col.as_any()
                .downcast_ref::<UInt8Array>()
                .unwrap()
                .value(row) as u64,
        ),
        DataType::UInt16 => u64_value(
            col.as_any()
                .downcast_ref::<UInt16Array>()
                .unwrap()
                .value(row) as u64,
        ),
        DataType::UInt32 => u64_value(
            col.as_any()
                .downcast_ref::<UInt32Array>()
                .unwrap()
                .value(row) as u64,
        ),
        DataType::UInt64 => u64_value(
            col.as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap()
                .value(row),
        ),
        DataType::Float32 => f64_value(
            col.as_any()
                .downcast_ref::<Float32Array>()
                .unwrap()
                .value(row) as f64,
        ),
        DataType::Float64 => f64_value(
            col.as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .value(row),
        ),
        _ => Value::String(fallback.value(row).to_string()),
    }
}

fn i64_value(n: i64) -> serde_yaml::Value {
    serde_yaml::Value::Number(serde_yaml::Number::from(n))
}

fn u64_value(n: u64) -> serde_yaml::Value {
    serde_yaml::Value::Number(serde_yaml::Number::from(n))
}

fn f64_value(n: f64) -> serde_yaml::Value {
    serde_yaml::Value::Number(serde_yaml::Number::from(n))
}
