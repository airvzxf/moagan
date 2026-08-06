//! Tests for D.17.7: CSV summary writer.

use crate::telemetry::csv_summary::{SketchSummaryRow, write_sketches_summary};

#[test]
fn csv_summary_writes_header_and_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let rows: Vec<SketchSummaryRow> = vec![
        ("minimax/MiniMax-M3".into(), 4, 1200),
        ("openai/gpt-4o".into(), 2, 800),
    ];
    write_sketches_summary(tmp.path(), &rows).unwrap();

    let path = tmp.path().join("telemetry").join("sketches_summary.csv");
    let content = std::fs::read_to_string(&path).unwrap();
    let mut lines = content.lines();
    assert_eq!(lines.next().unwrap(), "model,sketch_count,total_tokens");
    let row1 = lines.next().unwrap();
    assert!(row1.contains("minimax/MiniMax-M3"), "got: {row1}");
    assert!(row1.contains(",4,"), "got: {row1}");
    assert!(row1.ends_with(",1200"), "got: {row1}");
    assert!(lines.next().unwrap().contains("openai/gpt-4o"));
}
