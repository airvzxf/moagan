//! `moagan discover` command line: the no-op flags announced as
//! removed in v0.20.0 are gone.

use clap::Parser;
use clap::error::ErrorKind;
use moagan::cli::Cli;

fn parse(extra: &[&str]) -> Result<Cli, clap::Error> {
    let mut args = vec!["moagan", "discover", "--provider", "mock", "--prompt", "x"];
    args.extend_from_slice(extra);
    Cli::try_parse_from(args)
}

#[test]
fn cluster_threshold_is_an_unknown_argument() {
    let err = parse(&["--cluster-threshold", "0.5"])
        .err()
        .map(|e| e.kind());
    assert_eq!(err, Some(ErrorKind::UnknownArgument));
}

#[test]
fn cache_facets_is_an_unknown_argument() {
    let err = parse(&["--cache-facets"]).err().map(|e| e.kind());
    assert_eq!(err, Some(ErrorKind::UnknownArgument));
}

#[test]
fn a_plain_discover_command_still_parses() {
    assert!(parse(&["--matrix-spec", "auth=oauth,api-key"]).is_ok());
}
