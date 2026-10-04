//! Keeps the packaged man page (`contrib/debian/ferrofin-server.1`) in step
//! with the clap `Cli`: the page is rendered from it, and this fails when the
//! committed copy differs. Regenerate with
//! `FERROFIN_UPDATE_MANPAGE=1 cargo test -p ferrofin-server --test manpage`.

use clap::CommandFactory as _;
use clap_mangen::Man;
use clap_mangen::roff::{Roff, bold, italic, roman};
use ferrofin_server::config::Cli;

const PAGE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../contrib/debian/ferrofin-server.1"
);

/// Every `render_*` call writes roff's apostrophe preamble; the page keeps one.
const PREAMBLE: &str = ".ie \\n(.g .ds Aq \\(aq\n.el .ds Aq '\n";

fn render() -> String {
    // No VERSION section and no version or date in the title: the crate version
    // is not the release version, and the page would churn on every bump.
    // Written by hand because roff drops the empty date argument from `.TH`.
    let man = Man::new(Cli::command());
    let mut out = b".TH FERROFIN-SERVER 1 \"\" Ferrofin \"Ferrofin Manual\"\n".to_vec();
    man.render_name_section(&mut out).expect("render name");
    man.render_synopsis_section(&mut out)
        .expect("render synopsis");

    let mut description = Roff::new();
    description
        .control("SH", ["DESCRIPTION"])
        .text([roman(
            "Ferrofin is a Rust implementation of the Jellyfin media server. It \
             speaks the same HTTP API, so existing Jellyfin web, mobile and TV \
             clients connect to it unchanged.",
        )])
        .control("PP", [])
        .text([roman(
            "Pointed at a data directory holding a Jellyfin database, it adopts \
             it in place. Adoption is one-way: Ferrofin copies the original to \
             jellyfin.db.pre-ferrofin, then migrates it, and the result may not \
             open under Jellyfin again.",
        )]);
    description.to_writer(&mut out).expect("render description");

    man.render_options_section(&mut out)
        .expect("render options");

    let mut extra = Roff::new();
    extra
        .control("SH", ["ENVIRONMENT"])
        .text([
            roman("Every setting has a "),
            bold("FERROFIN_*"),
            roman(" environment variable. Flags override the environment, which overrides "),
            italic("config.toml"),
            roman(". The full list is in "),
            italic("/usr/share/doc/ferrofin/CONFIG.md"),
            roman("."),
        ])
        .control("SH", ["FILES"])
        .control("TP", [])
        .text([italic("/etc/ferrofin/config.toml")])
        .text([roman(
            "Configuration read by the packaged systemd unit. It may hold the \
             bootstrap admin password, so it is root:ferrofin, mode 0640.",
        )])
        .control("TP", [])
        .text([italic("/var/lib/ferrofin")])
        .text([roman(
            "Data directory of the packaged service: the database, cache, logs \
             and plugins.",
        )])
        .control("TP", [])
        .text([italic("/lib/systemd/system/ferrofin.service")])
        .text([roman(
            "The systemd unit. The package does not enable or start it.",
        )])
        .control("SH", ["SEE ALSO"])
        .text([
            italic("/usr/share/doc/ferrofin/INSTALL.md"),
            roman(", "),
            italic("/usr/share/doc/ferrofin/UPGRADING.md"),
            roman(", https://github.com/mangoleaf/ferrofin"),
        ]);
    extra.to_writer(&mut out).expect("render extra sections");

    let body = String::from_utf8(out).expect("roff output is UTF-8");
    format!("{PREAMBLE}{}", body.replace(PREAMBLE, ""))
}

#[test]
fn packaged_man_page_matches_the_cli() {
    let page = render();
    if std::env::var_os("FERROFIN_UPDATE_MANPAGE").is_some() {
        std::fs::write(PAGE, &page).expect("write the man page");
    }
    let committed = std::fs::read_to_string(PAGE).unwrap_or_default();
    assert!(
        committed == page,
        "{PAGE} is out of date with the CLI; regenerate it with \
         FERROFIN_UPDATE_MANPAGE=1 cargo test -p ferrofin-server --test manpage"
    );
}
