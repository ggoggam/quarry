//! Reading a statement's response into a [`BatchSink`].

use std::collections::VecDeque;
use std::io::{Cursor, Read};
use std::sync::Arc;

use anyhow::anyhow;
use arrow::array::StringArray;
use arrow::buffer::Buffer;
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::ipc::reader::{FileReader, StreamDecoder};
use arrow::record_batch::RecordBatch;
use quarry_core::BatchSink;
use quarry_core::result::{empty_schema, native_type};

use crate::convert;
use crate::error;
use crate::http::Reply;

/// How a response's rows ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// The server sent everything.
    Complete,
    /// The sink asked to stop; the response was dropped unread.
    Stopped,
}

/// Bytes read from the socket at a time.
const READ_CHUNK: usize = 256 * 1024;
/// How much of the end of the body to keep for finding an exception.
const TAIL: usize = 64 * 1024;
/// Batches smaller than this that arrive together are merged.
const SMALL_BATCH: usize = 8 * 1024;
/// Batches larger than this are sliced, so the grid gets rows sooner.
const LARGE_BATCH: usize = 128 * 1024;

pub(crate) fn read_reply(
    reply: Reply,
    described: Option<&[(String, String)]>,
    sink: &mut dyn BatchSink,
) -> anyhow::Result<Ended> {
    let format = reply.header("X-ClickHouse-Format").unwrap_or("").to_owned();
    let tag = reply
        .header("X-ClickHouse-Exception-Tag")
        .map(str::to_owned);
    let body = reply.response.into_body().into_reader();
    let mut body = Tail::new(body);
    let outcome = match format.as_str() {
        "ArrowStream" => read_arrow_stream(&mut body, described, sink),
        "Arrow" => read_arrow_file(&mut body, described, sink),
        _ => read_text(&mut body, &format, sink),
    };
    match outcome {
        Ok(ended) => Ok(ended),
        Err(err) => {
            // The server may have written its exception into the stream
            // after part of the result; that says more than a decode error.
            body.drain();
            match error::trailing_exception(body.tail(), tag.as_deref()) {
                Some(message) => Err(anyhow!(message)),
                None => Err(err),
            }
        }
    }
}

/// A reader that remembers the last bytes it returned.
struct Tail<R> {
    inner: R,
    tail: VecDeque<u8>,
}

impl<R: Read> Tail<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            tail: VecDeque::with_capacity(TAIL),
        }
    }

    fn tail(&mut self) -> &[u8] {
        self.tail.make_contiguous()
    }

    /// Read what is left of a failed response, keeping its end.
    fn drain(&mut self) {
        let mut buf = vec![0; 16 * 1024];
        let mut left = 1 << 20;
        while left > 0 {
            match self.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => left -= n.min(left),
            }
        }
    }
}

impl<R: Read> Read for Tail<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let bytes = &buf[..n];
        let keep = bytes.len().min(TAIL);
        let overflow = (self.tail.len() + keep).saturating_sub(TAIL);
        self.tail.drain(..overflow);
        self.tail.extend(&bytes[bytes.len() - keep..]);
        Ok(n)
    }
}

/// Shapes batches for the sink: text instead of bytes, the engine's type
/// names on fields, tiny batches merged and huge ones sliced.
struct Shaper<'s> {
    sink: &'s mut dyn BatchSink,
    schema: Option<SchemaRef>,
    pending: Vec<RecordBatch>,
    pending_rows: usize,
}

impl<'s> Shaper<'s> {
    fn new(sink: &'s mut dyn BatchSink) -> Self {
        Self {
            sink,
            schema: None,
            pending: Vec::new(),
            pending_rows: 0,
        }
    }

    fn start(&mut self, arrow: &Schema, described: Option<&[(String, String)]>) {
        if self.schema.is_some() {
            return;
        }
        let schema = Arc::new(output_schema(arrow, described));
        self.sink.schema(schema.clone());
        self.schema = Some(schema);
    }

    /// Queue a batch; hand over what is queued once it is big enough.
    fn push(&mut self, batch: RecordBatch) -> anyhow::Result<bool> {
        let schema = self.schema.clone().expect("schema first");
        let batch = convert::to_text(&batch, &schema)?;
        if batch.num_rows() == 0 {
            return Ok(true);
        }
        self.pending_rows += batch.num_rows();
        self.pending.push(batch);
        if self.pending_rows >= SMALL_BATCH {
            return self.flush();
        }
        Ok(true)
    }

    /// Hand over everything queued. `false` when the sink wants no more.
    fn flush(&mut self) -> anyhow::Result<bool> {
        if self.pending.is_empty() {
            return Ok(true);
        }
        let schema = self.schema.clone().expect("schema first");
        let batches = std::mem::take(&mut self.pending);
        self.pending_rows = 0;
        let batch = if batches.len() == 1 {
            batches.into_iter().next().expect("one batch")
        } else {
            concat_batches(&schema, &batches)?
        };
        let mut offset = 0;
        while offset < batch.num_rows() {
            let len = (batch.num_rows() - offset).min(LARGE_BATCH);
            let part = if offset == 0 && len == batch.num_rows() {
                batch.clone()
            } else {
                batch.slice(offset, len)
            };
            if self.sink.batch(part).is_break() {
                return Ok(false);
            }
            offset += len;
        }
        Ok(true)
    }

    fn finish(mut self) -> anyhow::Result<Ended> {
        if self.schema.is_none() {
            // A statement without a result (DDL and the like).
            self.sink.schema(empty_schema());
        }
        Ok(if self.flush()? {
            Ended::Complete
        } else {
            Ended::Stopped
        })
    }
}

/// The schema the sink sees: byte columns as text, and each field tagged
/// with ClickHouse's type when `DESCRIBE` gave a matching column list.
pub(crate) fn output_schema(arrow: &Schema, described: Option<&[(String, String)]>) -> Schema {
    let described = described.filter(|columns| columns.len() == arrow.fields().len());
    let fields: Vec<Field> = arrow
        .fields()
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let field = field
                .as_ref()
                .clone()
                .with_data_type(convert::text_type(field.data_type()));
            match described.map(|columns| &columns[i]) {
                Some((_, ty)) => native_type(field, ty.clone()),
                None => field,
            }
        })
        .collect();
    Schema::new_with_metadata(fields, arrow.metadata().clone())
}

fn read_arrow_stream(
    body: &mut impl Read,
    described: Option<&[(String, String)]>,
    sink: &mut dyn BatchSink,
) -> anyhow::Result<Ended> {
    let mut decoder = StreamDecoder::new();
    let mut shaper = Shaper::new(sink);
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        let n = body.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        let mut buffer = Buffer::from(&chunk[..n]);
        while !buffer.is_empty() {
            let batch = decoder.decode(&mut buffer)?;
            if let Some(schema) = decoder.schema() {
                shaper.start(&schema, described);
            }
            if let Some(batch) = batch
                && !shaper.push(batch)?
            {
                return Ok(Ended::Stopped);
            }
        }
        // About to wait for the network: hand over what has arrived.
        if !shaper.flush()? {
            return Ok(Ended::Stopped);
        }
    }
    decoder
        .finish()
        .map_err(|err| anyhow!("the result stream ended early: {err}"))?;
    shaper.finish()
}

/// `FORMAT Arrow` (the file format) needs the whole body to read.
fn read_arrow_file(
    body: &mut impl Read,
    described: Option<&[(String, String)]>,
    sink: &mut dyn BatchSink,
) -> anyhow::Result<Ended> {
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes)?;
    let mut shaper = Shaper::new(sink);
    if bytes.is_empty() {
        return shaper.finish();
    }
    let reader = FileReader::try_new(Cursor::new(bytes), None)?;
    shaper.start(&reader.schema(), described);
    for batch in reader {
        if !shaper.push(batch?)? {
            return Ok(Ended::Stopped);
        }
    }
    shaper.finish()
}

/// Any other format: the body as text, one row per line. Statements
/// without a result (DDL) send no format header and an empty body.
fn read_text(
    body: &mut impl Read,
    format: &str,
    sink: &mut dyn BatchSink,
) -> anyhow::Result<Ended> {
    let name = if format.is_empty() { "result" } else { format };
    let schema = Arc::new(Schema::new(vec![native_type(
        Field::new(name, DataType::Utf8, false),
        "String",
    )]));
    let mut schema_sent = false;
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let n = body.read(&mut chunk)?;
        let at_end = n == 0;
        carry.extend_from_slice(&chunk[..n]);
        let cut = if at_end {
            carry.len()
        } else {
            match carry.iter().rposition(|&b| b == b'\n') {
                Some(at) => at + 1,
                None => continue,
            }
        };
        let complete: Vec<u8> = carry.drain(..cut).collect();
        let text = String::from_utf8_lossy(&complete);
        let mut lines: Vec<&str> = text.split('\n').collect();
        if text.is_empty() || text.ends_with('\n') {
            lines.pop();
        }
        if !lines.is_empty() {
            if !schema_sent {
                sink.schema(schema.clone());
                schema_sent = true;
            }
            let lines = lines.iter().map(|l| l.strip_suffix('\r').unwrap_or(l));
            let column = StringArray::from_iter_values(lines);
            let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(column)])?;
            if sink.batch(batch).is_break() {
                return Ok(Ended::Stopped);
            }
        }
        if at_end {
            if !schema_sent {
                // Nothing came back: a statement without a result.
                sink.schema(empty_schema());
            }
            return Ok(Ended::Complete);
        }
    }
}

/// Undo TabSeparated escaping (`\t`, `\n`, `\\`, ...).
pub(crate) fn unescape_tsv(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use super::*;

    #[derive(Default)]
    struct Collect {
        schema: Option<SchemaRef>,
        batches: Vec<RecordBatch>,
        stop_after: Option<usize>,
    }

    impl BatchSink for Collect {
        fn schema(&mut self, schema: SchemaRef) {
            self.schema = Some(schema);
        }
        fn batch(&mut self, batch: RecordBatch) -> ControlFlow<()> {
            self.batches.push(batch);
            match self.stop_after {
                Some(n) if self.batches.len() >= n => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            }
        }
    }

    #[test]
    fn unescapes_tab_separated_fields() {
        assert_eq!(unescape_tsv(r"a\tb\\c\nd"), "a\tb\\c\nd");
        assert_eq!(unescape_tsv(r"it\'s"), "it's");
    }

    #[test]
    fn reads_text_one_row_per_line() {
        let mut sink = Collect::default();
        let body = b"a,1\r\nb,2\n\"c\",3\n".to_vec();
        let ended = read_text(&mut Cursor::new(body), "CSV", &mut sink).unwrap();
        assert_eq!(ended, Ended::Complete);
        let schema = sink.schema.unwrap();
        assert_eq!(schema.field(0).name(), "CSV");
        let rows: Vec<String> = sink
            .batches
            .iter()
            .flat_map(|b| {
                let col = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
                col.iter()
                    .map(|v| v.unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(rows, ["a,1", "b,2", "\"c\",3"]);

        let mut sink = Collect::default();
        read_text(&mut Cursor::new(Vec::new()), "", &mut sink).unwrap();
        assert_eq!(sink.schema.unwrap().fields().len(), 0);
    }

    #[test]
    fn tags_fields_with_described_types() {
        let arrow = Schema::new(vec![
            Field::new("s", DataType::Binary, true),
            Field::new("n", DataType::UInt8, false),
        ]);
        let described = vec![
            ("s".to_owned(), "Nullable(String)".to_owned()),
            ("n".to_owned(), "UInt8".to_owned()),
        ];
        let schema = output_schema(&arrow, Some(&described));
        assert_eq!(schema.field(0).data_type(), &DataType::Utf8);
        assert_eq!(
            quarry_core::result::field_type_label(schema.field(0)),
            "Nullable(String)"
        );
        // A column list that does not match is ignored.
        let schema = output_schema(&arrow, Some(&described[..1]));
        assert_eq!(
            quarry_core::result::field_type_label(schema.field(1)),
            "utinyint"
        );
    }

    #[test]
    fn decodes_an_arrow_stream_and_merges_small_batches() {
        use arrow::array::Int32Array;
        use arrow::ipc::writer::StreamWriter;

        let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int32, false)]));
        let mut bytes = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut bytes, &schema).unwrap();
            for i in 0..10 {
                let batch = RecordBatch::try_new(
                    schema.clone(),
                    vec![Arc::new(Int32Array::from(vec![i; 100]))],
                )
                .unwrap();
                writer.write(&batch).unwrap();
            }
            writer.finish().unwrap();
        }
        let mut sink = Collect::default();
        let ended = read_arrow_stream(&mut Cursor::new(bytes.clone()), None, &mut sink).unwrap();
        assert_eq!(ended, Ended::Complete);
        assert_eq!(sink.batches.len(), 1, "small batches read together merge");
        assert_eq!(sink.batches[0].num_rows(), 1000);

        // A stream cut short is an error, not a short result.
        let mut sink = Collect::default();
        let cut = bytes[..bytes.len() - 20].to_vec();
        assert!(read_arrow_stream(&mut Cursor::new(cut), None, &mut sink).is_err());
    }
}
