//! `gpui-updater-release`: the release steps of the `gpui-updater-release`
//! crate for shell scripts and CI workflows. Run with `--help` for every
//! option.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use gpui_updater_release::sparkle::{
    self, BundleCheck, CodesignOptions, DeltaOptions, EmbedOptions, SparkleDelta, SparkleRelease,
    SparkleSettings,
};
use gpui_updater_release::{
    Appcast, AppcastItem, Classifier, Critical, Enclosure, FeedLayout, ManifestBuilder,
    MinisignKey, MinisignOptions, PublishedAt, ReleaseNotes, SignaturePolicy, SignatureStyle,
    SparkleKey, Unmatched, ensure_sparkle_version, write_manifest,
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(
    name = "gpui-updater-release",
    version,
    about = "Release-side tooling for gpui-updater"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write the static JSON manifest (`latest.json`) for a release.
    Manifest(Box<ManifestArgs>),
    /// Write Sparkle appcasts for a release's macOS archives, with deltas.
    Appcast(Box<AppcastArgs>),
    /// Sign files.
    #[command(subcommand)]
    Sign(SignCommand),
    /// Create or inspect keys.
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Fetch, embed, configure, sign, and check Sparkle.framework; make
    /// Sparkle deltas.
    #[command(subcommand)]
    Sparkle(Box<SparkleCommand>),
    /// Native `files-bsdiff-v1` deltas for the built-in engine.
    #[cfg(feature = "delta")]
    #[command(subcommand)]
    Delta(DeltaCommand),
}

/// Where the update host keeps channels and releases.
#[derive(Args, Clone)]
struct LayoutArgs {
    /// Public base URL of the update host.
    #[arg(long, env = "GPUI_UPDATER_BASE_URL")]
    base_url: String,
    /// Release channel.
    #[arg(long, default_value = "stable")]
    channel: String,
    /// Channel directory template (`{channel}`).
    #[arg(long, default_value = "channels/{channel}")]
    channel_dir: String,
    /// Manifest file name template.
    #[arg(long, default_value = "latest.json")]
    manifest_name: String,
    /// Appcast file name template (`{arch}`; leave it out for one feed).
    #[arg(long, default_value = "appcast-{arch}.xml")]
    appcast_name: String,
    /// Release directory template (`{tag}`, `{version}`).
    #[arg(long, default_value = "releases/{tag}")]
    release_dir: String,
}

impl LayoutArgs {
    fn layout(&self) -> FeedLayout {
        FeedLayout::new(&self.base_url, &self.channel)
            .channel_dir(&self.channel_dir)
            .manifest_name(&self.manifest_name)
            .appcast_name(&self.appcast_name)
            .release_dir(&self.release_dir)
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum PolicyArg {
    Required,
    IfPresent,
    Ignore,
}

#[derive(Clone, Copy, ValueEnum)]
enum StyleArg {
    Url,
    Inline,
    Both,
}

#[derive(Args)]
#[allow(clippy::struct_excessive_bools)] // One flag per command-line switch.
struct ManifestArgs {
    #[command(flatten)]
    layout: LayoutArgs,
    /// Release tag, for example `v1.2.3`; the version is the tag without `v`.
    #[arg(long)]
    tag: String,
    /// Directories with the release files (not recursive). Delta files named
    /// `<asset>.from-<version>.delta` are attached to their asset.
    #[arg(long, required = true)]
    dist: Vec<PathBuf>,
    /// Where to write the manifest.
    #[arg(long)]
    out: PathBuf,
    /// URL prefix the files download from, instead of the layout's release
    /// directory.
    #[arg(long)]
    download_base: Option<String>,
    /// `app_id`.
    #[arg(long)]
    app_id: Option<String>,
    /// `release_url`: the release's web page.
    #[arg(long)]
    release_url: Option<String>,
    /// File with the release notes, embedded as `notes`.
    #[arg(long)]
    notes_file: Option<PathBuf>,
    /// `notes_url`.
    #[arg(long)]
    notes_url: Option<String>,
    /// `published_at` (RFC 3339); default now.
    #[arg(long, conflicts_with = "no_published_at")]
    published_at: Option<String>,
    /// Leave out `published_at`.
    #[arg(long)]
    no_published_at: bool,
    /// Set `expires` this many days after `published_at`.
    #[arg(long)]
    expires_days: Option<i64>,
    /// Oldest supported OS version, as `os=version` (repeatable), for
    /// example `macos=13.0`.
    #[arg(long, value_parser = key_value)]
    min_os: Vec<(String, String)>,
    /// Accepted architectures in file names (repeatable); default arm64,
    /// aarch64, `x86_64`, x64, universal.
    #[arg(long)]
    arch: Vec<String>,
    /// List only these operating systems (repeatable).
    #[arg(long)]
    include_os: Vec<String>,
    /// Leave out this operating system (repeatable).
    #[arg(long)]
    exclude_os: Vec<String>,
    /// Fail unless an asset exists for `os` or `os/arch` (repeatable).
    #[arg(long)]
    require: Vec<String>,
    /// Whether files need `<file>.minisig` signatures.
    #[arg(long, value_enum, default_value = "required")]
    signatures: PolicyArg,
    /// How signatures appear in the manifest.
    #[arg(long, value_enum, default_value = "url")]
    signature_style: StyleArg,
    /// Environment variable with a Sparkle Ed25519 key; adds
    /// `ed25519_signature` to every asset and delta.
    #[arg(long)]
    ed25519_key_env: Option<String>,
    /// Fail on files no rule classifies, instead of skipping them.
    #[arg(long)]
    strict: bool,
    /// Leave out `sha256`.
    #[arg(long)]
    no_sha256: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum NotesFormat {
    Markdown,
    Html,
    PlainText,
}

#[derive(Args)]
#[allow(clippy::struct_excessive_bools)] // One flag per command-line switch.
struct AppcastArgs {
    #[command(flatten)]
    layout: LayoutArgs,
    /// Release tag, for example `v1.2.3`.
    #[arg(long)]
    tag: String,
    /// Directory with the release archives and Sparkle deltas.
    #[arg(long)]
    dist: PathBuf,
    /// Directory to write the feeds into.
    #[arg(long)]
    out_dir: PathBuf,
    /// Archive format to offer.
    #[arg(long, default_value = "dmg", value_parser = ["dmg", "zip"])]
    format: String,
    /// Architectures that must have an archive (repeatable); default every
    /// one found.
    #[arg(long)]
    require_arch: Vec<String>,
    /// Environment variable with the Sparkle private key (`generate_keys -x`).
    #[arg(long, default_value = "SPARKLE_PRIVATE_KEY")]
    key_env: String,
    /// The public key the bundles embed; the private key must match it.
    #[arg(long, env = "SPARKLE_PUBLIC_KEY")]
    public_key: Option<String>,
    /// Channel title.
    #[arg(long)]
    title: String,
    /// Channel description (`{channel}` is replaced).
    #[arg(long)]
    description: Option<String>,
    /// Channel language.
    #[arg(long)]
    language: Option<String>,
    /// Item title (`{title}`, `{version}`).
    #[arg(long, default_value = "{title} {version}")]
    item_title: String,
    /// `sparkle:version` (`CFBundleVersion`); default the tag's version.
    #[arg(long)]
    bundle_version: Option<String>,
    /// `sparkle:shortVersionString`; default the tag's version.
    #[arg(long)]
    short_version: Option<String>,
    /// Allowed number of dot-separated parts in `sparkle:version`, `N` or
    /// `MIN..MAX`.
    #[arg(long, default_value = "3", value_parser = parse_parts)]
    version_parts: (usize, usize),
    /// Skip the check that Sparkle orders the version correctly.
    #[arg(long)]
    any_version: bool,
    /// `sparkle:minimumSystemVersion`.
    #[arg(long)]
    minimum_system_version: Option<String>,
    /// `<link>` and `sparkle:fullReleaseNotesLink`.
    #[arg(long)]
    release_url: Option<String>,
    /// `sparkle:releaseNotesLink`.
    #[arg(long)]
    release_notes_link: Option<String>,
    /// File with release notes to embed.
    #[arg(long)]
    notes_file: Option<PathBuf>,
    /// Format of `--notes-file`.
    #[arg(long, value_enum, default_value = "markdown")]
    notes_format: NotesFormat,
    /// `sparkle:channel`.
    #[arg(long)]
    sparkle_channel: Option<String>,
    /// Mark the update critical for everyone.
    #[arg(long, conflicts_with = "critical_below")]
    critical: bool,
    /// Mark the update critical below this version.
    #[arg(long)]
    critical_below: Option<String>,
    /// `sparkle:minimumAutoupdateVersion`.
    #[arg(long)]
    minimum_autoupdate_version: Option<String>,
    /// `sparkle:phasedRolloutInterval` in seconds.
    #[arg(long)]
    phased_rollout_interval: Option<u64>,
    /// Do not attach the `.sparkle.delta` files found in `--dist`.
    #[arg(long)]
    no_deltas: bool,
}

#[derive(Subcommand)]
enum SignCommand {
    /// Write `<file>.minisig` for each file, bound to the release version.
    Minisign {
        /// Environment variable with the secret key file's contents.
        #[arg(long, default_value = "MINISIGN_SECRET_KEY")]
        key_env: String,
        /// Environment variable with the key's password, if encrypted.
        #[arg(long)]
        password_env: Option<String>,
        /// The public key clients trust; the secret key must match it.
        #[arg(long, env = "MINISIGN_PUBLIC_KEY")]
        public_key: Option<String>,
        /// Version to bind into the trusted comment.
        #[arg(long)]
        version: Option<String>,
        /// Trusted comment, instead of the default one.
        #[arg(long, conflicts_with = "version")]
        trusted_comment: Option<String>,
        /// Files to sign.
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Print `<signature>  <file>` (`sparkle:edSignature`) for each file.
    Sparkle {
        /// Environment variable with the Sparkle private key.
        #[arg(long, default_value = "SPARKLE_PRIVATE_KEY")]
        key_env: String,
        /// Files to sign.
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
}

#[derive(Subcommand)]
enum KeysCommand {
    /// Generate a Sparkle key: prints the private key, then the public key.
    GenerateSparkle,
    /// Generate an unencrypted minisign key: writes the secret key file and
    /// prints the public key.
    GenerateMinisign {
        /// Where to write the secret key.
        #[arg(long)]
        out: PathBuf,
    },
    /// Print the public key of the Sparkle private key in an environment
    /// variable.
    SparklePublic {
        /// Environment variable with the Sparkle private key.
        #[arg(long, default_value = "SPARKLE_PRIVATE_KEY")]
        key_env: String,
    },
}

/// Which Sparkle release to use.
#[derive(Args)]
struct SparkleDistArgs {
    /// Directory to cache the release archive and unpack it in.
    #[arg(long, default_value = "target/sparkle")]
    cache: PathBuf,
    /// Sparkle version; default the pinned one.
    #[arg(long, requires = "sparkle_sha256")]
    sparkle_version: Option<String>,
    /// SHA-256 of that version's archive.
    #[arg(long, requires = "sparkle_version")]
    sparkle_sha256: Option<String>,
    /// Download from this URL instead of GitHub.
    #[arg(long)]
    sparkle_url: Option<String>,
}

impl SparkleDistArgs {
    fn fetch(&self) -> Result<sparkle::SparkleDistribution> {
        let mut release = match (&self.sparkle_version, &self.sparkle_sha256) {
            (Some(version), Some(sha)) => SparkleRelease::new(version, sha),
            _ => SparkleRelease::pinned(),
        };
        if let Some(url) = &self.sparkle_url {
            release = release.url(url);
        }
        Ok(release.fetch(&self.cache)?)
    }
}

/// Sparkle's `Info.plist` keys.
#[derive(Args)]
struct SettingsArgs {
    /// `SUFeedURL`.
    #[arg(long)]
    feed_url: String,
    /// `SUPublicEDKey`.
    #[arg(long, env = "SPARKLE_PUBLIC_KEY")]
    public_key: String,
    /// `SUEnableAutomaticChecks`.
    #[arg(long)]
    automatic_checks: Option<bool>,
    /// `SUAllowsAutomaticUpdates`.
    #[arg(long)]
    allows_automatic_updates: Option<bool>,
    /// `SUAutomaticallyUpdate`.
    #[arg(long)]
    automatically_update: Option<bool>,
    /// `SUScheduledCheckInterval` in seconds.
    #[arg(long)]
    scheduled_check_interval: Option<u64>,
    /// `SUEnableSystemProfiling`.
    #[arg(long)]
    system_profiling: Option<bool>,
    /// `SUVerifyUpdateBeforeExtraction`.
    #[arg(long)]
    verify_before_extraction: Option<bool>,
    /// `SURequireSignedFeed`.
    #[arg(long)]
    require_signed_feed: Option<bool>,
    /// `SUShowReleaseNotes`.
    #[arg(long)]
    show_release_notes: Option<bool>,
    /// Any other key, as `KEY=VALUE` (`true`/`false` and integers are typed).
    #[arg(long, value_parser = key_value)]
    set: Vec<(String, String)>,
    /// Do not require `CFBundleVersion` to equal the short version.
    #[arg(long)]
    allow_any_bundle_version: bool,
}

impl SettingsArgs {
    fn settings(&self) -> Result<SparkleSettings> {
        let mut s = SparkleSettings::new(&self.feed_url, &self.public_key)?;
        s.enable_automatic_checks = self.automatic_checks;
        s.allows_automatic_updates = self.allows_automatic_updates;
        s.automatically_update = self.automatically_update;
        s.scheduled_check_interval = self.scheduled_check_interval;
        s.enable_system_profiling = self.system_profiling;
        s.verify_update_before_extraction = self.verify_before_extraction;
        s.require_signed_feed = self.require_signed_feed;
        s.show_release_notes = self.show_release_notes;
        for (key, value) in &self.set {
            s = s.set(key, plist_value(value));
        }
        if self.allow_any_bundle_version {
            s = s.allow_any_bundle_version();
        }
        Ok(s)
    }
}

#[derive(Subcommand)]
enum SparkleCommand {
    /// Download, check, and unpack the Sparkle release; prints its directory.
    Fetch(SparkleDistArgs),
    /// Embed Sparkle.framework into an app and configure `Info.plist`.
    Embed {
        /// The `.app` bundle.
        #[arg(long)]
        app: PathBuf,
        #[command(flatten)]
        dist: SparkleDistArgs,
        #[command(flatten)]
        settings: SettingsArgs,
        /// Keep the XPC services (sandboxed apps).
        #[arg(long)]
        keep_xpc_services: bool,
        /// Do not copy Sparkle's license into the framework.
        #[arg(long)]
        no_license: bool,
    },
    /// Configure Sparkle's `Info.plist` keys only.
    Configure {
        /// The `.app` bundle.
        #[arg(long)]
        app: PathBuf,
        #[command(flatten)]
        settings: SettingsArgs,
    },
    /// Sign Sparkle's code inside the app, inside out. Sign the app after.
    Codesign {
        /// The `.app` bundle.
        #[arg(long)]
        app: PathBuf,
        /// Signing identity, or `-` for ad-hoc.
        #[arg(long, env = "APPLE_SIGNING_IDENTITY")]
        identity: String,
        /// Keychain to find the identity in.
        #[arg(long)]
        keychain: Option<PathBuf>,
        /// Do not request a secure timestamp.
        #[arg(long)]
        no_timestamp: bool,
        /// Do not enable the hardened runtime.
        #[arg(long)]
        no_runtime: bool,
    },
    /// Print the code paths to sign, inside out.
    CodePaths {
        /// The `.app` bundle.
        #[arg(long)]
        app: PathBuf,
    },
    /// Check an embedded, configured, and signed bundle.
    Verify {
        /// The `.app` bundle.
        #[arg(long)]
        app: PathBuf,
        /// `SUFeedURL` to expect.
        #[arg(long, requires = "public_key")]
        feed_url: Option<String>,
        /// `SUPublicEDKey` to expect.
        #[arg(long, requires = "feed_url")]
        public_key: Option<String>,
        /// Allow the XPC services.
        #[arg(long)]
        allow_xpc_services: bool,
        /// Skip `codesign` checks (unsigned development builds).
        #[arg(long)]
        skip_signatures: bool,
    },
    /// Make a Sparkle delta with `BinaryDelta`, verified, plus `<delta>.json`
    /// for `appcast`.
    Delta(SparkleDeltaArgs),
}

#[derive(Args)]
struct SparkleDeltaArgs {
    /// The old app (`.app`, or a `.dmg` to copy it from).
    #[arg(long)]
    old: PathBuf,
    /// The new app (`.app`, or a `.dmg`).
    #[arg(long)]
    new: PathBuf,
    /// Delta file, or a directory to write
    /// `<archive>.from-<version>.sparkle.delta` into.
    #[arg(long)]
    out: PathBuf,
    /// Archive name for the file name when `--out` is a directory;
    /// default the `--new` file name.
    #[arg(long)]
    archive: Option<String>,
    #[command(flatten)]
    dist: SparkleDistArgs,
    /// Delta format version; the old app's Sparkle must read it.
    #[arg(long)]
    format_version: Option<u32>,
    /// `lzma`, `lzfse`, `lz4`, `bzip2`, or `default`.
    #[arg(long)]
    compression: Option<String>,
    /// Skip applying the delta to check it.
    #[arg(long)]
    no_verify: bool,
}

#[cfg(feature = "delta")]
#[derive(Subcommand)]
enum DeltaCommand {
    /// Make a native delta, verified.
    Create {
        /// The old build (`.app` directory or executable).
        #[arg(long)]
        old: PathBuf,
        /// The new build.
        #[arg(long)]
        new: PathBuf,
        /// Version of the old build.
        #[arg(long)]
        from: String,
        /// Version of the new build.
        #[arg(long)]
        to: String,
        /// Delta file, or a directory to write `<asset>.from-<from>.delta`
        /// into (needs `--asset`).
        #[arg(long)]
        out: PathBuf,
        /// Asset the delta rebuilds, for the file name.
        #[arg(long)]
        asset: Option<String>,
        /// Skip applying the delta to check it.
        #[arg(long)]
        no_verify: bool,
    },
    /// Print which earlier releases to build deltas from.
    Bases {
        /// How many of the newest releases to always include.
        #[arg(long, default_value = "3")]
        direct: usize,
        /// Earlier releases, newest first.
        versions: Vec<String>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result {
    match cli.command {
        Command::Manifest(args) => manifest(&args),
        Command::Appcast(args) => appcast(&args),
        Command::Sign(cmd) => sign(cmd),
        Command::Keys(cmd) => keys(cmd),
        Command::Sparkle(cmd) => sparkle_cmd(*cmd),
        #[cfg(feature = "delta")]
        Command::Delta(cmd) => delta_cmd(cmd),
    }
}

fn manifest(args: &ManifestArgs) -> Result {
    let layout = args.layout.layout();
    let mut builder = match &args.download_base {
        Some(base) => ManifestBuilder::new(&args.tag, base)
            .tag(&args.tag)
            .channel(&layout.channel),
        None => ManifestBuilder::for_layout(&layout, &args.tag),
    };
    let mut classifier = Classifier::default();
    for (os, version) in &args.min_os {
        classifier = classifier.minimum_os_version(os, version);
    }
    if !args.arch.is_empty() {
        let arches: Vec<&str> = args.arch.iter().map(String::as_str).collect();
        classifier = classifier.arches(&arches);
    }
    builder = builder
        .classifier(classifier)
        .signatures(match args.signatures {
            PolicyArg::Required => SignaturePolicy::Required,
            PolicyArg::IfPresent => SignaturePolicy::IfPresent,
            PolicyArg::Ignore => SignaturePolicy::Ignore,
        })
        .signature_style(match args.signature_style {
            StyleArg::Url => SignatureStyle::Url,
            StyleArg::Inline => SignatureStyle::Inline,
            StyleArg::Both => SignatureStyle::Both,
        })
        .unmatched(if args.strict {
            Unmatched::Error
        } else {
            Unmatched::Skip
        });
    if let Some(v) = &args.app_id {
        builder = builder.app_id(v);
    }
    if let Some(v) = &args.release_url {
        builder = builder.release_url(v);
    }
    if let Some(path) = &args.notes_file {
        builder = builder.notes(read(path)?);
    }
    if let Some(v) = &args.notes_url {
        builder = builder.notes_url(v);
    }
    if args.no_published_at {
        builder = builder.published_at(PublishedAt::Omit);
    } else if let Some(at) = &args.published_at {
        builder = builder.published_at(PublishedAt::At(at.clone()));
    }
    if let Some(days) = args.expires_days {
        builder = builder.expires_after(time::Duration::days(days));
    }
    if !args.include_os.is_empty() {
        builder = builder.include_os(args.include_os.clone());
    }
    for os in &args.exclude_os {
        builder = builder.exclude_os(os);
    }
    for target in &args.require {
        builder = match target.split_once('/') {
            Some((os, arch)) => builder.require_target(os, arch),
            None => builder.require_os(target),
        };
    }
    if let Some(var) = &args.ed25519_key_env {
        builder = builder.ed25519_key(SparkleKey::from_base64(&env(var)?)?);
    }
    if args.no_sha256 {
        builder = builder.without_sha256();
    }
    for dir in &args.dist {
        builder = builder.scan_dir(dir)?;
    }
    let manifest = builder.build()?;
    write_manifest(&manifest, &args.out)?;
    println!(
        "{} → {} ({} assets)",
        args.out.display(),
        layout.manifest_url(),
        manifest.assets.len()
    );
    Ok(())
}

fn appcast(args: &AppcastArgs) -> Result {
    let layout = args.layout.layout();
    let version = args.tag.strip_prefix('v').unwrap_or(&args.tag).to_string();
    let bundle_version = args
        .bundle_version
        .clone()
        .unwrap_or_else(|| version.clone());
    if !args.any_version {
        ensure_sparkle_version(&bundle_version, args.version_parts.0, args.version_parts.1)?;
    }
    let key = SparkleKey::from_base64(&env(&args.key_env)?)?;
    if let Some(public) = &args.public_key {
        key.expect_public_key(public)?;
    }
    let notes = args
        .notes_file
        .as_deref()
        .map(read)
        .transpose()?
        .map(|text| match args.notes_format {
            NotesFormat::Markdown => ReleaseNotes::Markdown(text),
            NotesFormat::Html => ReleaseNotes::Html(text),
            NotesFormat::PlainText => ReleaseNotes::PlainText(text),
        });
    let archives = macos_archives(args)?;
    let deltas = if args.no_deltas {
        Vec::new()
    } else {
        sparkle_deltas(&args.dist)?
    };

    std::fs::create_dir_all(&args.out_dir)?;
    for (arch, archive) in &archives {
        let name = file_name(archive)?;
        let feed_url = layout.appcast_url(arch);
        let enclosure = Enclosure::for_file(archive, layout.file_url(&args.tag, name), Some(&key))?;
        let mut item = AppcastItem::new(&bundle_version, enclosure)
            .title(
                args.item_title
                    .replace("{title}", &args.title)
                    .replace("{version}", &version),
            )
            .short_version(
                args.short_version
                    .clone()
                    .unwrap_or_else(|| version.clone()),
            );
        if let Some(v) = &args.minimum_system_version {
            item = item.minimum_system_version(v);
        }
        if let Some(url) = &args.release_url {
            item = item.link(url).full_release_notes_link(url);
        }
        if let Some(url) = &args.release_notes_link {
            item = item.release_notes_link(url);
        }
        if let Some(notes) = &notes {
            item = item.notes(notes.clone());
        }
        if let Some(channel) = &args.sparkle_channel {
            item = item.channel(channel);
        }
        if args.critical {
            item = item.critical(Critical::Always);
        } else if let Some(below) = &args.critical_below {
            item = item.critical(Critical::Below(below.clone()));
        }
        if let Some(v) = &args.minimum_autoupdate_version {
            item = item.minimum_autoupdate_version(v);
        }
        if let Some(seconds) = args.phased_rollout_interval {
            item = item.phased_rollout_interval(seconds);
        }
        let prefix = format!("{name}.from-");
        for delta in deltas
            .iter()
            .filter(|d| file_name(&d.path).is_ok_and(|n| n.starts_with(&prefix)))
        {
            if delta.to_bundle_version != bundle_version {
                return Err(format!(
                    "{} produces {}, not {bundle_version}",
                    delta.path.display(),
                    delta.to_bundle_version
                )
                .into());
            }
            let url = layout.file_url(&args.tag, file_name(&delta.path)?);
            item = item.delta(delta.enclosure(url, Some(&key))?);
        }
        let mut feed = Appcast::new(&args.title, &feed_url).item(item);
        if let Some(d) = &args.description {
            feed = feed.description(d.replace("{channel}", &layout.channel));
        }
        if let Some(l) = &args.language {
            feed = feed.language(l);
        }
        let path = layout.appcast_path(arch);
        let out = args.out_dir.join(path.rsplit('/').next().unwrap_or(&path));
        feed.write(&out)?;
        println!("{} → {feed_url}", out.display());
    }
    Ok(())
}

/// The macOS archive of each architecture in `--dist`, by architecture.
fn macos_archives(args: &AppcastArgs) -> Result<Vec<(String, PathBuf)>> {
    let classifier = Classifier::default()
        .only_os(&["macos"])
        .only_formats(&[&args.format]);
    let mut archives: Vec<(String, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&args.dist)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(arch) = classifier.classify(name).and_then(|c| c.arch) else {
            continue;
        };
        if let Some((_, other)) = archives.iter().find(|(a, _)| *a == arch) {
            return Err(format!(
                "two {arch} archives: {} and {}",
                other.display(),
                path.display()
            )
            .into());
        }
        archives.push((arch, path));
    }
    archives.sort();
    for arch in &args.require_arch {
        if !archives.iter().any(|(a, _)| a == arch) {
            return Err(format!("no {arch} .{} in {}", args.format, args.dist.display()).into());
        }
    }
    if archives.is_empty() {
        return Err(format!("no macOS .{} in {}", args.format, args.dist.display()).into());
    }
    if !args.layout.appcast_name.contains("{arch}") && archives.len() > 1 {
        return Err(
            "several architectures but one appcast name; add {arch} to --appcast-name".into(),
        );
    }
    Ok(archives)
}

fn sparkle_deltas(dir: &Path) -> Result<Vec<SparkleDelta>> {
    Ok(sparkle::delta::find_deltas(dir)?)
}

fn sign(cmd: SignCommand) -> Result {
    match cmd {
        SignCommand::Minisign {
            key_env,
            password_env,
            public_key,
            version,
            trusted_comment,
            files,
        } => {
            let password = password_env.as_deref().map(env).transpose()?;
            let key = MinisignKey::from_box(&env(&key_env)?, password)?;
            if let Some(public) = &public_key {
                key.expect_public_key(public)?;
            }
            let mut options =
                version.map_or_else(MinisignOptions::default, MinisignOptions::version);
            options.trusted_comment = trusted_comment;
            for file in &files {
                println!("{}", key.sign_file_to_sidecar(file, &options)?.display());
            }
        }
        SignCommand::Sparkle { key_env, files } => {
            let key = SparkleKey::from_base64(&env(&key_env)?)?;
            for file in &files {
                println!("{}  {}", key.sign_file(file)?, file.display());
            }
        }
    }
    Ok(())
}

fn keys(cmd: KeysCommand) -> Result {
    match cmd {
        KeysCommand::GenerateSparkle => {
            let key = SparkleKey::generate();
            println!("{}", key.to_base64());
            println!("{}", key.public_key_base64());
        }
        KeysCommand::GenerateMinisign { out } => {
            if out.exists() {
                return Err(format!("{} exists; not overwriting a key", out.display()).into());
            }
            let key = MinisignKey::generate()?;
            std::fs::write(&out, key.to_box()?)?;
            println!("{}", key.public_key_base64());
        }
        KeysCommand::SparklePublic { key_env } => {
            println!(
                "{}",
                SparkleKey::from_base64(&env(&key_env)?)?.public_key_base64()
            );
        }
    }
    Ok(())
}

fn sparkle_cmd(cmd: SparkleCommand) -> Result {
    match cmd {
        SparkleCommand::Fetch(dist) => println!("{}", dist.fetch()?.root().display()),
        SparkleCommand::Embed {
            app,
            dist,
            settings,
            keep_xpc_services,
            no_license,
        } => {
            let dist = dist.fetch()?;
            let mut options = EmbedOptions::default();
            if keep_xpc_services {
                options = options.keep_xpc_services();
            }
            if no_license {
                options = options.without_license();
            }
            let framework = sparkle::embed(&app, &dist, &options)?;
            settings
                .settings()?
                .apply(&app.join("Contents/Info.plist"))?;
            println!("Sparkle {} → {}", dist.version()?, framework.display());
        }
        SparkleCommand::Configure { app, settings } => {
            settings
                .settings()?
                .apply(&app.join("Contents/Info.plist"))?;
        }
        SparkleCommand::Codesign {
            app,
            identity,
            keychain,
            no_timestamp,
            no_runtime,
        } => {
            let mut options = CodesignOptions::new(identity);
            options.keychain = keychain;
            options.timestamp = !no_timestamp;
            options.hardened_runtime = !no_runtime;
            sparkle::codesign_sparkle(&app, &options)?;
        }
        SparkleCommand::CodePaths { app } => {
            for path in sparkle::code_inside_out(&app) {
                println!("{}", path.display());
            }
        }
        SparkleCommand::Verify {
            app,
            feed_url,
            public_key,
            allow_xpc_services,
            skip_signatures,
        } => {
            let settings = match (feed_url, public_key) {
                (Some(url), Some(key)) => Some(SparkleSettings::new(url, &key)?),
                _ => None,
            };
            let mut check = BundleCheck::new(settings);
            check.expect_no_xpc_services = !allow_xpc_services;
            check.signatures = !skip_signatures;
            sparkle::verify_bundle(&app, &check)?;
            println!("{}: ok", app.display());
        }
        SparkleCommand::Delta(args) => sparkle_delta(args)?,
    }
    Ok(())
}

fn sparkle_delta(args: SparkleDeltaArgs) -> Result {
    let SparkleDeltaArgs {
        old,
        new,
        out,
        archive,
        dist,
        format_version,
        compression,
        no_verify,
    } = args;
    let dist = dist.fetch()?;
    let scratch = tempfile::tempdir()?;
    let old_app = app_from(&old, &scratch.path().join("old"))?;
    let new_app = app_from(&new, &scratch.path().join("new"))?;
    let out = if out.is_dir() {
        let archive = match archive {
            Some(archive) => archive,
            None => file_name(&new)?.to_string(),
        };
        let from = sparkle::delta::inspect(&old_app)?.bundle_version;
        out.join(sparkle::delta::delta_file_name(&archive, &from))
    } else {
        out
    };
    let mut options = DeltaOptions::default();
    options.format_version = format_version;
    options.compression = compression;
    options.verify = !no_verify;
    let delta = sparkle::delta::create(&dist, &old_app, &new_app, &out, &options)?;
    delta.write_metadata()?;
    println!(
        "{}: {} → {}, {} bytes{}",
        out.display(),
        delta.from.bundle_version,
        delta.to_bundle_version,
        delta.length,
        if no_verify { "" } else { ", verified" }
    );
    Ok(())
}

/// An `.app` as given, or the one copied out of a `.dmg` into `scratch`.
fn app_from(path: &Path, scratch: &Path) -> Result<PathBuf> {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("dmg"))
    {
        Ok(sparkle::extract_app(path, scratch, None)?)
    } else {
        Ok(path.to_path_buf())
    }
}

#[cfg(feature = "delta")]
fn delta_cmd(cmd: DeltaCommand) -> Result {
    use gpui_updater_core::Version;
    use gpui_updater_release::native_delta::{self, NativeDeltaOptions};
    match cmd {
        DeltaCommand::Create {
            old,
            new,
            from,
            to,
            out,
            asset,
            no_verify,
        } => {
            let from = Version::parse(from.trim_start_matches('v'))?;
            let to = Version::parse(to.trim_start_matches('v'))?;
            let out = if out.is_dir() {
                let asset = asset.ok_or("--out is a directory; name the asset with --asset")?;
                native_delta::delta_path(&out, &asset, &from)
            } else {
                out
            };
            let mut options = NativeDeltaOptions::default();
            options.verify = !no_verify;
            let delta = native_delta::create(&old, &new, &from, &to, &out, &options)?;
            println!(
                "{}: {} → {}, {} bytes ({} copied, {} patched, {} added){}",
                out.display(),
                delta.info.from,
                delta.info.to,
                delta.length,
                delta.info.copied,
                delta.info.patched,
                delta.info.added,
                if no_verify { "" } else { ", verified" }
            );
        }
        DeltaCommand::Bases { direct, versions } => {
            let previous = versions
                .iter()
                .map(|v| Version::parse(v.trim_start_matches('v')))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for version in native_delta::bases(&previous, direct) {
                println!("{version}");
            }
        }
    }
    Ok(())
}

fn env(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| format!("set {name}").into())
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()).into())
}

fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no UTF-8 file name", path.display()).into())
}

fn key_value(s: &str) -> std::result::Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VALUE, not {s:?}"))
}

fn parse_parts(s: &str) -> std::result::Result<(usize, usize), String> {
    let parse = |p: &str| p.parse::<usize>().map_err(|e| format!("{p:?}: {e}"));
    match s.split_once("..") {
        Some((min, max)) => Ok((parse(min)?, parse(max)?)),
        None => parse(s).map(|n| (n, n)),
    }
}

fn plist_value(s: &str) -> plist::Value {
    match s {
        "true" => plist::Value::Boolean(true),
        "false" => plist::Value::Boolean(false),
        _ => s.parse::<i64>().map_or_else(
            |_| plist::Value::String(s.to_string()),
            |n| plist::Value::Integer(n.into()),
        ),
    }
}
