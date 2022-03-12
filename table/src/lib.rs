//! Generate markdown comparison tables from
//! [Cargo Criterion](https://github.com/bheisler/cargo-criterion) benchmark output.
//!
//! Currently, the tool is limited to Github Flavored Markdown (GFM), but adding
//! new output types is simple.
//!
//! ## Generated Markdown Example
//!
//! [Benchmark Report](example/README.md)

/// This module holds the various formatters that can be used to format the output
pub mod formatter;

use std::cmp::max;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, ErrorKind, Read};
use std::ops::Div;
use std::path::Path;

use anyhow::anyhow;
use flexstr::{flex_fmt, FlexStr, IntoFlex, ToCase, ToFlex, ToFlexStr};
use indexmap::map::Entry;
use indexmap::IndexMap;
use serde::Deserialize;

// Trick to test README samples (from: https://github.com/rust-lang/cargo/issues/383#issuecomment-720873790)
#[cfg(doctest)]
mod test_readme {
    macro_rules! external_doc_test {
        ($x:expr) => {
            #[doc = $x]
            extern "C" {}
        };
    }

    external_doc_test!(include_str!("../../README.md"));
}

// Starting capacity for the String buffer used to build the page
const BUFFER_CAPACITY: usize = 65535;

// *** Raw JSON Data Structs ***

// NOTE: These were shamelessly copied (with translation) from:
// https://github.com/bheisler/cargo-criterion/blob/main/src/message_formats/json.rs

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ConfidenceInterval {
    estimate: f64,
    lower_bound: f64,
    upper_bound: f64,
    unit: FlexStr,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct Throughput {
    per_iteration: u64,
    unit: FlexStr,
}

#[derive(Debug, Deserialize)]
enum ChangeType {
    NoChange,
    Improved,
    Regressed,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ChangeDetails {
    mean: ConfidenceInterval,
    median: ConfidenceInterval,

    change: ChangeType,
}

/// Raw Criterion benchmark data
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct BenchmarkComplete {
    id: FlexStr,
    report_directory: FlexStr,
    iteration_count: Vec<u64>,
    measured_values: Vec<f64>,
    unit: FlexStr,

    throughput: Vec<Throughput>,

    typical: ConfidenceInterval,
    mean: ConfidenceInterval,
    median: ConfidenceInterval,
    median_abs_dev: ConfidenceInterval,
    slope: Option<ConfidenceInterval>,

    change: Option<ChangeDetails>,
}

/// Raw Criterion benchmark group data
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct BenchmarkGroupComplete {
    group_name: FlexStr,
    benchmarks: Vec<FlexStr>,
    report_directory: FlexStr,
}

/// Enum that can hold either raw benchmark or benchmark group data
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum RawCriterionData {
    /// Raw benchmark data
    Benchmark(Box<BenchmarkComplete>),
    /// Raw benchmark group data
    BenchmarkGroup(Box<BenchmarkGroupComplete>),
}

impl RawCriterionData {
    /// Load raw Criterion JSON data from the reader
    pub fn from_reader(r: impl Read) -> serde_json::error::Result<Vec<Self>> {
        let reader = BufReader::new(r);
        let mut de = serde_json::Deserializer::from_reader(reader);
        let mut data_vec = Vec::new();

        loop {
            match RawCriterionData::deserialize(&mut de) {
                Ok(data) => data_vec.push(data),
                Err(err) if err.is_eof() => break,
                Err(err) => return Err(err),
            }
        }

        Ok(data_vec)
    }
}

// *** Tables Config ***

#[derive(Default, Deserialize)]
/// Configuration file format loaded by Serde
pub struct TablesConfig {
    pub comments: Option<FlexStr>,
    pub table_comments: HashMap<FlexStr, FlexStr>,
}

impl TablesConfig {
    /// Try to load the config from the given reader
    pub fn try_load_config(r: impl Read) -> anyhow::Result<Self> {
        let mut reader = BufReader::new(r);
        let mut buffer = String::with_capacity(16384);
        reader.read_to_string(&mut buffer)?;

        let config: TablesConfig = toml::from_str(&buffer)?;
        Ok(config)
    }
}

// *** Criterion Data ***

// ### Column Info ###

#[derive(Clone, Debug)]
pub struct ColumnInfo {
    pub name: FlexStr,
    pub max_width: usize,
}

impl ColumnInfo {
    #[inline]
    pub fn new(name: FlexStr, width: usize) -> Self {
        Self {
            name,
            max_width: width,
        }
    }

    #[inline]
    fn update_info(&mut self, width: usize) {
        self.max_width = max(self.max_width, width);
    }
}

// ### Time Unit ###

#[derive(Clone, Copy, Debug)]
pub enum TimeUnit {
    Second(f64),
    Millisecond(f64),
    Microsecond(f64),
    Nanosecond(f64),
    Picosecond(f64),
}

impl TimeUnit {
    pub fn try_new(time: f64, unit: &str) -> anyhow::Result<Self> {
        match unit {
            "ms" if time > 1000.0 => Self::try_new(time / 1000.0, "s"),
            "us" if time > 1000.0 => Self::try_new(time / 1000.0, "ms"),
            "ns" if time > 1000.0 => Self::try_new(time / 1000.0, "us"),
            "ps" if time > 1000.0 => Self::try_new(time / 1000.0, "ns"),
            "s" => Ok(TimeUnit::Second(time)),
            "ms" => Ok(TimeUnit::Millisecond(time)),
            "us" => Ok(TimeUnit::Microsecond(time)),
            "ns" => Ok(TimeUnit::Nanosecond(time)),
            "ps" => Ok(TimeUnit::Picosecond(time)),
            _ => Err(anyhow!("Unrecognized time unit: {unit}")),
        }
    }

    #[inline]
    pub fn width(&self) -> usize {
        self.to_flex_str().chars().count()
    }

    fn as_picoseconds(&self) -> f64 {
        match *self {
            TimeUnit::Second(s) => s * 1_000_000_000_000.0,
            TimeUnit::Millisecond(ms) => ms * 1_000_000_000.0,
            TimeUnit::Microsecond(us) => us * 1_000_000.0,
            TimeUnit::Nanosecond(ns) => ns * 1_000.0,
            TimeUnit::Picosecond(ps) => ps,
        }
    }
}

impl Div for TimeUnit {
    type Output = f64;

    fn div(self, rhs: Self) -> Self::Output {
        let unit1 = self.as_picoseconds();
        let unit2 = rhs.as_picoseconds();
        unit1 / unit2
    }
}

impl ToFlexStr for TimeUnit {
    fn to_flex_str(&self) -> FlexStr {
        match self {
            TimeUnit::Second(time) => flex_fmt!("{time:.2} s"),
            TimeUnit::Millisecond(time) => flex_fmt!("{time:.2} ms"),
            TimeUnit::Microsecond(time) => flex_fmt!("{time:.2} us"),
            TimeUnit::Nanosecond(time) => flex_fmt!("{time:.2} ns"),
            TimeUnit::Picosecond(time) => flex_fmt!("{time:.2} ps"),
        }
    }
}

// ### Percent ###

#[derive(Clone, Copy, Debug, Default)]
pub struct Comparison(f64);

impl Comparison {
    #[inline]
    pub fn width(self) -> usize {
        self.to_flex_str().chars().count()
    }
}

impl ToFlexStr for Comparison {
    fn to_flex_str(&self) -> FlexStr {
        if self.0 > 1.0 {
            flex_fmt!("{:.2}x faster", self.0)
        } else if self.0 < 1.0 {
            flex_fmt!("{:.2}x slower", 1.0 / self.0)
        } else {
            flex_fmt!("{:.2}x", self.0)
        }
    }
}

// #### Column ###

#[derive(Clone, Debug)]
struct Column {
    #[allow(dead_code)]
    name: FlexStr,
    time_unit: TimeUnit,
    pct: Comparison,
}

impl Column {
    pub fn new(name: FlexStr, time_unit: TimeUnit, first_col_time: Option<TimeUnit>) -> Self {
        let pct = match first_col_time {
            Some(first_col_time) => Comparison(first_col_time / time_unit),
            None => Comparison(1.0),
        };

        Self {
            name,
            time_unit,
            pct,
        }
    }

    // This returns the "width" of the resulting text in chars. Since we don't know how it will be
    // formatted we return width of: TimeUnit + Percent. Any additional spaces or formatting chars
    // are not considered and must be added by the formatter
    #[inline]
    pub fn width(&self) -> usize {
        self.time_unit.width() + self.pct.width()
    }
}

// ### Row ###

#[derive(Clone, Debug)]
struct Row {
    name: FlexStr,
    column_data: IndexMap<FlexStr, Column>,
}

impl Row {
    #[inline]
    pub fn new(name: FlexStr) -> Self {
        Self {
            name,
            column_data: Default::default(),
        }
    }

    // NOTE: The 'first' column here reflects the first column seen for THIS row NOT for the whole table
    // This means our timings COULD be based off different columns in different rows
    fn first_column_time(&self) -> Option<TimeUnit> {
        self.column_data
            .first()
            .map(|(_, Column { time_unit, .. })| *time_unit)
    }

    fn add_column(&mut self, name: FlexStr, time_unit: TimeUnit) -> anyhow::Result<&Column> {
        let first_time = self.first_column_time();

        match self.column_data.entry(name.clone()) {
            Entry::Occupied(_) => Err(anyhow!("Duplicate column: {name}")),
            Entry::Vacant(entry) => {
                let col = Column::new(name, time_unit, first_time);
                Ok(entry.insert(col))
            }
        }
    }
}

// ### Column Info Map ###

#[derive(Clone, Debug, Default)]
struct ColumnInfoVec(Vec<ColumnInfo>);

impl ColumnInfoVec {
    pub fn update_column_info(&mut self, idx: usize, name: FlexStr, width: usize) {
        match self.0.iter_mut().find(|col| col.name == name) {
            Some(col_info) => col_info.update_info(width),
            None => self.0.insert(idx, ColumnInfo::new(name, width)),
        }
    }
}

// ### Table ###

#[derive(Clone, Debug)]
struct Table {
    name: FlexStr,
    columns: ColumnInfoVec,
    rows: IndexMap<FlexStr, Row>,
}

impl Table {
    #[inline]
    pub fn new(name: FlexStr) -> Self {
        Self {
            name,
            columns: Default::default(),
            rows: Default::default(),
        }
    }

    pub fn add_column_data(
        &mut self,
        idx: usize,
        column_name: FlexStr,
        row_name: FlexStr,
        time: TimeUnit,
    ) -> anyhow::Result<()> {
        // Assume we have a blank named first column just for holding the row name
        self.columns
            .update_column_info(0, Default::default(), row_name.chars().count());

        let row = self.get_row(row_name);
        let col = row.add_column(column_name.clone(), time)?;

        // Use either the width of the data or the name, whichever is larger
        let width = max(col.width(), column_name.chars().count());
        self.columns.update_column_info(idx, column_name, width);
        Ok(())
    }

    fn get_row(&mut self, name: FlexStr) -> &mut Row {
        match self.rows.entry(name.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Row::new(name)),
        }
    }
}

// ### Column Position ###

#[derive(Default)]
struct ColumnPosition(IndexMap<FlexStr, usize>);

impl ColumnPosition {
    pub fn next_idx(&mut self, row_name: FlexStr) -> usize {
        match self.0.entry(row_name) {
            Entry::Occupied(mut entry) => {
                *entry.get_mut() += 1;
                *entry.get()
            }
            Entry::Vacant(entry) => *entry.insert(1),
        }
    }
}

// ### Criterion Table Data ###

/// Fully processed Criterion benchmark data ready for formatting
#[derive(Clone, Debug)]
pub struct CriterionTableData {
    tables: IndexMap<FlexStr, Table>,
}

impl CriterionTableData {
    /// Build table data from the input raw Criterion data
    pub fn from_raw(raw_data: &[RawCriterionData]) -> anyhow::Result<Self> {
        let mut data = Self {
            tables: Default::default(),
        };

        data.build_from_raw_data(raw_data)?;
        Ok(data)
    }

    fn build_from_raw_data(&mut self, raw_data: &[RawCriterionData]) -> anyhow::Result<()> {
        let mut col_pos = ColumnPosition::default();

        for item in raw_data {
            // We only process benchmark data - skip anything else
            if let RawCriterionData::Benchmark(bm) = item {
                // Break the id into table, column, and row respectively
                let mut parts: Vec<FlexStr> = bm.id.split('/').map(|s| s.to_flex()).collect();
                if parts.len() < 2 {
                    return Err(anyhow::anyhow!("Malformed id: {}", &bm.id));
                }

                let (table_name, column_name) = (parts.remove(0), parts.remove(0));
                // If we don't have a row name then we will work with a blank row name
                let row_name = if !parts.is_empty() {
                    parts.remove(0)
                } else {
                    "".into()
                };

                // Find our table, calculate our timing, and add data to our column
                let table = self.get_table(table_name);
                let time_unit = TimeUnit::try_new(bm.typical.estimate, &bm.typical.unit)?;

                let idx = col_pos.next_idx(row_name.clone());
                table.add_column_data(idx, column_name, row_name, time_unit)?;
            }
        }

        Ok(())
    }

    fn get_table(&mut self, name: FlexStr) -> &mut Table {
        match self.tables.entry(name.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Table::new(name)),
        }
    }

    fn encode_key(s: &FlexStr) -> FlexStr {
        s.replace(' ', "_").into_flex().to_lower()
    }

    pub fn make_tables(&self, mut f: impl Formatter, config: &TablesConfig) -> String {
        let mut buffer = String::with_capacity(BUFFER_CAPACITY);

        // Start of doc
        let table_names: Vec<_> = self.tables.keys().collect();
        f.start(&mut buffer, config.comments.as_ref(), &table_names);

        for table in self.tables.values() {
            let col_info = &table.columns.0;

            if let Some(first_col) = col_info.first() {
                // Start of table
                let comments = config.table_comments.get(&Self::encode_key(&table.name));
                f.start_table(&mut buffer, &table.name, comments, col_info);

                for row in table.rows.values() {
                    // Start of row
                    f.start_row(&mut buffer, &row.name, first_col.max_width);

                    for col in &col_info[1..] {
                        match row.column_data.get(&col.name) {
                            // Used column
                            Some(col_data) => f.used_column(
                                &mut buffer,
                                col_data.time_unit,
                                col_data.pct,
                                col.max_width,
                            ),
                            // Unused column
                            None => f.unused_column(&mut buffer, col.max_width),
                        }
                    }

                    // End of row
                    f.end_row(&mut buffer);
                }

                // End of table
                f.end_table(&mut buffer);
            }
        }

        // End of doc
        f.end(&mut buffer);

        buffer
    }
}

// *** Formatter ***

pub trait Formatter {
    fn start(&mut self, buffer: &mut String, comment: Option<&FlexStr>, tables: &[&FlexStr]);

    fn end(&mut self, buffer: &mut String);

    fn start_table(
        &mut self,
        buffer: &mut String,
        name: &FlexStr,
        comment: Option<&FlexStr>,
        columns: &[ColumnInfo],
    );

    fn end_table(&mut self, buffer: &mut String);

    fn start_row(&mut self, buffer: &mut String, name: &FlexStr, max_width: usize);

    fn end_row(&mut self, buffer: &mut String);

    fn used_column(
        &mut self,
        buffer: &mut String,
        time: TimeUnit,
        pct: Comparison,
        max_width: usize,
    );

    fn unused_column(&mut self, buffer: &mut String, max_width: usize);
}

// *** Functions ***

fn load_config(cfg_name: impl AsRef<Path>) -> anyhow::Result<TablesConfig> {
    match File::open(cfg_name) {
        // If the file exists, but it can't be deserialized then report that error
        Ok(f) => Ok(TablesConfig::try_load_config(f)?),
        // If file just isn't there then ignore and return a blank config
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(TablesConfig::default()),
        // Report any other I/O errors
        Err(err) => Err(err.into()),
    }
}

pub fn build_tables(
    read: impl Read,
    fmt: impl Formatter,
    cfg_name: impl AsRef<Path>,
) -> anyhow::Result<String> {
    let raw_data = RawCriterionData::from_reader(read)?;
    let data = CriterionTableData::from_raw(&raw_data)?;
    let config = load_config(cfg_name)?;
    Ok(data.make_tables(fmt, &config))
}
