//! Android's binary resource formats, written here because `aapt2` ships for x86-64 hosts only
//! and this one is arm64.
//!
//! `compile` reads the XML under `android/` — a real manifest and a real `res/` tree — and writes
//! what goes in the APK: a binary AndroidManifest.xml, a `resources.arsc`, and the compiled or
//! copied files the table points at. `gen-table` refreshes the framework ids it resolves against.

mod apk;
mod arsc;
mod chunk;
mod compile;
mod framework;
mod vector;
mod xml;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use arsc::{Entry, Localized, Res, Type};
use compile::Symbols;

/// Types in the order their ids are assigned, which is what `@type/name` resolves through.
/// New types go at the end, so the ids of the others never move.
const TYPES: [&str; 6] = ["color", "drawable", "mipmap", "style", "string", "plurals"];
const VALUES: &str = "values.xml";
/// `res/values/values.xml`: every value, in the default configuration.
const DEFAULT_VALUES: &str = "values";
/// `res/values-ar/values.xml` and the like: translations of the default folder's text.
const LOCALIZED_VALUES: &str = "values-";

#[derive(Parser)]
#[command(about = "Android binary resources without aapt2", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile android/ into the files an APK carries.
    Compile(CompileArgs),
    /// Zip a staged directory into an unsigned APK, with resources.arsc stored and aligned as
    /// API 30+ requires.
    Package(PackageArgs),
    /// Turn `javap -constants` output on android.R$attr and R$style into a lookup table, so
    /// `android:` names in XML resolve to ids instead of being hand-copied constants.
    GenTable(GenTableArgs),
    /// Write `R.java`: every resource's id as a constant, `R.string.mute`, from the same table
    /// `compile` writes — so Java names resources the compiler checks, not strings it looks up.
    RClass(RClassArgs),
    /// Write the UI's SVG icons into res/drawable as `<vector>` drawables. The results are
    /// checked in; run it again when an icon changes.
    Vectors(VectorsArgs),
}

#[derive(Parser)]
struct VectorsArgs {
    /// Where the SVGs are.
    #[arg(long, default_value = "assets/icons")]
    icons: PathBuf,
    /// The drawable folder to write into.
    #[arg(long, default_value = "android/res/drawable")]
    out: PathBuf,
}

#[derive(Parser)]
struct RClassArgs {
    /// The directory holding res/.
    #[arg(long, default_value = "android")]
    source: PathBuf,
    /// The Java package R belongs to: the app's.
    #[arg(long)]
    package: String,
    /// The source root to write `<package path>/R.java` under.
    #[arg(long)]
    out: PathBuf,
}

#[derive(Parser)]
struct PackageArgs {
    /// The staged directory to zip.
    #[arg(long)]
    dir: PathBuf,
    /// The unsigned APK to write.
    #[arg(long)]
    out: PathBuf,
}

#[derive(Parser)]
struct CompileArgs {
    /// The directory holding AndroidManifest.xml and res/.
    #[arg(long, default_value = "android")]
    source: PathBuf,
    /// Where to stage the APK's contents.
    #[arg(long)]
    out: PathBuf,
    /// `name=value` for a `${name}` in the manifest. Repeatable.
    #[arg(long = "define", value_parser = parse_define)]
    defines: Vec<(String, String)>,
}

#[derive(Parser)]
struct GenTableArgs {
    /// The Rust file to write; javap's output arrives on stdin.
    #[arg(long)]
    out: String,
}

fn parse_define(text: &str) -> Result<(String, String), String> {
    text.split_once('=')
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| format!("{text} is not name=value"))
}

/// Everything `res/` declares, in one pass, so that a file can reference any other by name
/// whatever order they compile in.
struct Resources {
    types: Vec<Type>,
    symbols: Symbols,
    /// Source path to the path it takes inside the APK.
    files: Vec<(PathBuf, String)>,
}

fn collect(source: &Path) -> Result<Resources> {
    let res = source.join("res");
    let values = res.join(DEFAULT_VALUES).join(VALUES);
    let values_text = std::fs::read_to_string(&values).with_context(|| format!("reading {}", values.display()))?;

    // Colours and styles first: they are values rather than files, and everything else may
    // reference them.
    let mut colors = Vec::new();
    let mut styles = Vec::new();
    let mut files = Vec::new();
    let mut by_type: HashMap<&str, Vec<(String, String)>> = HashMap::new();
    for kind in ["drawable", "mipmap"] {
        let dir = res.join(kind);
        if !dir.is_dir() {
            continue;
        }
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let name = path.file_stem().context("a resource file needs a name")?.to_string_lossy().into_owned();
            let target = format!("res/{kind}/{}", path.file_name().unwrap_or_default().to_string_lossy());
            by_type.entry(kind).or_default().push((name, target.clone()));
            files.push((path, target));
        }
    }

    // The symbol table has to exist before any XML is compiled, so it is built from the names
    // alone — the file contents come after.
    let parsed = roxmltree::Document::parse(&values_text)?;
    let mut texts = Texts::default();
    for node in parsed.root_element().children().filter(roxmltree::Node::is_element) {
        let name = node.attribute("name").context("a resource needs a name")?.to_owned();
        match node.tag_name().name() {
            "color" => colors.push((name, node.text().unwrap_or_default().trim().to_owned())),
            "style" => styles.push(node),
            "string" | "plurals" => texts.add(node)?,
            other => bail!("{other} is not a value this tool understands"),
        }
    }

    let mut symbols = Symbols::new();
    let index = |kind: &str| TYPES.iter().position(|t| *t == kind).unwrap_or_default();
    for (i, (name, _)) in colors.iter().enumerate() {
        symbols.insert(format!("color/{name}"), Type::id(index("color"), i));
    }
    for kind in ["drawable", "mipmap"] {
        for (i, (name, _)) in by_type.get(kind).into_iter().flatten().enumerate() {
            symbols.insert(format!("{kind}/{name}"), Type::id(index(kind), i));
        }
    }
    for (i, node) in styles.iter().enumerate() {
        let name = node.attribute("name").unwrap_or_default();
        symbols.insert(format!("style/{name}"), Type::id(index("style"), i));
    }
    for (kind, names) in [("string", &texts.strings), ("plurals", &texts.plurals)] {
        for (i, (name, _)) in names.iter().enumerate() {
            symbols.insert(format!("{kind}/{name}"), Type::id(index(kind), i));
        }
    }
    let languages = translations(&res, &texts)?;

    // Now the values themselves, which may reference any of the above.
    let mut types = vec![
        Type {
            name: "color",
            entries: colors
                .iter()
                .map(|(name, text)| {
                    Ok(Entry {
                        name: name.clone(),
                        res: Res::Value { kind: chunk::TYPE_INT_COLOR_ARGB8, data: compile::color(text)? },
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            localized: Vec::new(),
        },
        file_type("drawable", &by_type),
        file_type("mipmap", &by_type),
        Type {
            name: "style",
            entries: styles
                .iter()
                .map(|node| {
                    let parent = match node.attribute("parent") {
                        Some(text) => compile::style_parent(text, &symbols)?,
                        None => 0,
                    };
                    let items = node
                        .children()
                        .filter(roxmltree::Node::is_element)
                        .map(|item| compile::style_item(item, &symbols))
                        .collect::<Result<Vec<_>>>()?;
                    Ok(Entry {
                        name: node.attribute("name").unwrap_or_default().to_owned(),
                        res: Res::Map { parent, items },
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            localized: Vec::new(),
        },
        text_type("string", &texts.strings, &languages, |texts| &texts.strings),
        text_type("plurals", &texts.plurals, &languages, |texts| &texts.plurals),
    ];
    types.retain(|t| !t.entries.is_empty() || t.name == "color");
    Ok(Resources { types, symbols, files })
}

/// `<string>` and `<plurals>` from one values file, in the order it declares them.
#[derive(Default)]
struct Texts {
    strings: Vec<(String, Res)>,
    plurals: Vec<(String, Res)>,
}

impl Texts {
    fn add(&mut self, node: roxmltree::Node) -> Result<()> {
        let name = node.attribute("name").context("a resource needs a name")?.to_owned();
        match node.tag_name().name() {
            "string" => self.strings.push((name, Res::Str(compile::string_text(node)?))),
            "plurals" => self.plurals.push((name, Res::Plural(compile::plural(node)?))),
            other => bail!("{other}: a translation holds only strings and plurals"),
        }
        Ok(())
    }
}

/// Every `values-<lang>/` folder, read against the default one: a translation of a name the
/// default does not declare is a mistake the lookup would never reach, so it is an error.
fn translations(res: &Path, defaults: &Texts) -> Result<Vec<([u8; 2], Texts)>> {
    let mut found = Vec::new();
    let mut folders: Vec<_> = std::fs::read_dir(res)?.collect::<std::io::Result<Vec<_>>>()?;
    folders.sort_by_key(std::fs::DirEntry::file_name);
    for folder in folders {
        let name = folder.file_name().to_string_lossy().into_owned();
        let Some(code) = name.strip_prefix(LOCALIZED_VALUES) else { continue };
        let Ok(language) = <[u8; 2]>::try_from(code.as_bytes()) else {
            bail!("{name}: only a two-letter language is understood here");
        };
        let path = folder.path().join(VALUES);
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut texts = Texts::default();
        for node in roxmltree::Document::parse(&text)?.root_element().children().filter(roxmltree::Node::is_element) {
            texts.add(node)?;
        }
        for (kind, mine, theirs) in
            [("string", &texts.strings, &defaults.strings), ("plurals", &texts.plurals, &defaults.plurals)]
        {
            if let Some((stray, _)) = mine.iter().find(|(name, _)| !theirs.iter().any(|(other, _)| other == name)) {
                bail!("{name}: {kind} {stray} has no default in {VALUES}");
            }
        }
        found.push((language, texts));
    }
    Ok(found)
}

/// A text type: the default entries, and each language's translation of them.
fn text_type(
    kind: &'static str,
    defaults: &[(String, Res)],
    languages: &[([u8; 2], Texts)],
    list: impl Fn(&Texts) -> &Vec<(String, Res)>,
) -> Type {
    Type {
        name: kind,
        entries: defaults.iter().map(|(name, res)| Entry { name: name.clone(), res: res.clone() }).collect(),
        localized: languages
            .iter()
            .map(|(language, texts)| Localized {
                language: *language,
                entries: defaults
                    .iter()
                    .map(|(name, _)| list(texts).iter().find(|(other, _)| other == name).map(|(_, res)| res.clone()))
                    .collect(),
            })
            .collect(),
    }
}

fn file_type(kind: &'static str, by_type: &HashMap<&str, Vec<(String, String)>>) -> Type {
    Type {
        name: kind,
        entries: by_type
            .get(kind)
            .into_iter()
            .flatten()
            .map(|(name, target)| Entry { name: name.clone(), res: Res::File(target.clone()) })
            .collect(),
        localized: Vec::new(),
    }
}

fn run(args: &CompileArgs) -> Result<()> {
    let defines: HashMap<String, String> = args.defines.iter().cloned().collect();
    let package = defines.get("package").context("--define package=... is required")?.clone();
    let resources = collect(&args.source)?;

    for (from, target) in &resources.files {
        let to = args.out.join(target);
        std::fs::create_dir_all(to.parent().context("a staged file needs a directory")?)?;
        if from.extension().is_some_and(|e| e == "xml") {
            let text = std::fs::read_to_string(from).with_context(|| format!("reading {}", from.display()))?;
            std::fs::write(&to, xml::encode(&compile::document(&text, &resources.symbols, &defines)?)?)?;
        } else {
            std::fs::copy(from, &to)?;
        }
    }

    let manifest_path = args.source.join("AndroidManifest.xml");
    let manifest =
        std::fs::read_to_string(&manifest_path).with_context(|| format!("reading {}", manifest_path.display()))?;
    std::fs::create_dir_all(&args.out)?;
    std::fs::write(
        args.out.join("AndroidManifest.xml"),
        xml::encode(&compile::document(&manifest, &resources.symbols, &defines)?)?,
    )?;
    std::fs::write(args.out.join("resources.arsc"), arsc::encode(&package, &resources.types)?)?;
    println!("{} resources -> {}", resources.symbols.len(), args.out.display());
    Ok(())
}

/// Reads `public static final int NAME = NUMBER;` lines, keyed by the class they came under, and
/// writes them sorted so the compiler can binary-search a name.
fn gen_table(args: &GenTableArgs) -> Result<()> {
    use std::io::Read as _;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    let mut class = String::new();
    let mut attrs: Vec<(String, u32)> = Vec::new();
    let mut styles: Vec<(String, u32)> = Vec::new();
    let mut named: Vec<Named> = VALUE_SOURCES
        .iter()
        .map(|(attr, flags, ..)| Named { attr, flags: *flags, rows: Vec::new() })
        .collect();
    for line in input.lines() {
        let line = line.trim();
        // `public [final] class android.R$attr {`, `public class android.content.pm.ServiceInfo …`
        if let Some((_, rest)) = line.split_once("class ")
            && line.starts_with("public ")
        {
            class = rest.split_whitespace().next().unwrap_or_default().to_owned();
            continue;
        }
        let Some(rest) = line.strip_prefix("public static final int ") else {
            continue;
        };
        let Some((name, value)) = rest.trim_end_matches(';').split_once(" = ") else {
            continue;
        };
        // Negative values (`FOREGROUND_SERVICE_TYPE_MANIFEST`) are markers, not flags.
        let Ok(id) = value.trim().parse::<u32>() else {
            continue;
        };
        let name = name.trim();
        match class.as_str() {
            "android.R$attr" => attrs.push((name.to_owned(), id)),
            "android.R$style" => styles.push((name.to_owned(), id)),
            _ => {
                for ((_, _, source, strip, groups), found) in VALUE_SOURCES.iter().zip(named.iter_mut()) {
                    if class != *source {
                        continue;
                    }
                    if let Some(word) = name.strip_prefix(strip)
                        && (groups.is_empty() || groups.iter().any(|group| word.starts_with(group)))
                    {
                        found.rows.push((xml_name(word), id));
                    }
                }
            }
        }
    }
    attrs.sort_by(|a, b| a.0.cmp(&b.0));
    styles.sort_by(|a, b| a.0.cmp(&b.0));
    for found in &mut named {
        found.rows.sort_by(|a, b| a.0.cmp(&b.0));
    }

    let mut out = String::from(
        "//! Generated by `just android-table` from android.jar. Do not edit.\n//!\n\
         //! Framework resource ids, so `android:` names in XML resolve without hand-copied\n\
         //! constants. Sorted, and looked up by binary search.\n\n",
    );
    for (table, rows) in [("ATTRS", &attrs), ("STYLES", &styles)] {
        out.push_str(&format!("pub const {table}: &[(&str, u32)] = &[\n"));
        for (name, id) in rows.iter() {
            out.push_str(&format!("    (\"{name}\", {id:#010x}),\n"));
        }
        out.push_str("];\n\n");
    }
    out.push_str(
        "/// The words an attribute's value may be written in, and the number each stands for, from\n\
         /// the SDK's own constants: `configChanges=\"keyboard|orientation\"` rather than `0x90`.\n\
         pub struct NamedValues {\n    \
             pub attr: &'static str,\n    \
             /// Flags combine with `|`; an enum is one word.\n    \
             pub flags: bool,\n    \
             pub words: &'static [(&'static str, u32)],\n\
         }\n\n\
         pub const VALUE_NAMES: &[NamedValues] = &[\n",
    );
    for Named { attr, flags, rows } in &named {
        out.push_str(&format!(
            "    NamedValues {{\n        attr: \"{attr}\",\n        flags: {flags},\n        words: &[\n"
        ));
        for (name, value) in rows {
            out.push_str(&format!("            (\"{name}\", {value:#x}),\n"));
        }
        out.push_str("        ],\n    },\n");
    }
    out.push_str("];\n\n");
    out.push_str(
        "pub fn lookup(table: &[(&str, u32)], name: &str) -> Option<u32> {\n    \
         table.binary_search_by(|(n, _)| (*n).cmp(name)).ok().map(|i| table[i].1)\n}\n",
    );
    std::fs::write(&args.out, out)?;
    println!("{} attrs, {} styles -> {}", attrs.len(), styles.len(), args.out);
    Ok(())
}

/// One attribute's words as they are gathered from javap's output.
struct Named {
    attr: &'static str,
    flags: bool,
    rows: Vec<(String, u32)>,
}

/// Manifest attributes whose values are named in the SDK as constants: the attribute, whether its
/// words combine as flags (or pick one, as an enum), the class that holds them, the prefix stripped
/// from each, and (if any) the groups kept after it.
const VALUE_SOURCES: [(&str, bool, &str, &str, &[&str]); 4] = [
    ("configChanges", true, "android.content.pm.ActivityInfo", "CONFIG_", &[]),
    ("launchMode", false, "android.content.pm.ActivityInfo", "LAUNCH_", &[]),
    ("foregroundServiceType", true, "android.content.pm.ServiceInfo", "FOREGROUND_SERVICE_TYPE_", &[]),
    // Not the masks or the navigation flag: only what a manifest may write.
    ("windowSoftInputMode", true, "android.view.WindowManager$LayoutParams", "SOFT_INPUT_", &["STATE_", "ADJUST_"]),
];

/// `KEYBOARD_HIDDEN` as the manifest writes it, `keyboardHidden`, and `ADJUST_RESIZE` as
/// `adjustResize`. `LAUNCH_MULTIPLE` is the one the manifest renames.
fn xml_name(word: &str) -> String {
    const RENAMED: [(&str, &str); 1] = [("MULTIPLE", "standard")];
    if let Some((_, xml)) = RENAMED.iter().find(|(sdk, _)| *sdk == word) {
        return (*xml).to_owned();
    }
    let mut out = String::new();
    for (i, part) in word.split('_').enumerate() {
        let lower = part.to_lowercase();
        let mut chars = lower.chars();
        if i == 0 {
            out.push_str(&lower);
        } else if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// `R.java`, one nested class per type, as aapt2 writes it. A style's dots become underscores,
/// which is also how Android's own R names them.
fn r_class(args: &RClassArgs) -> Result<()> {
    let resources = collect(&args.source)?;
    let mut by_type: Vec<(&str, Vec<(String, u32)>)> = TYPES.iter().map(|kind| (*kind, Vec::new())).collect();
    for (symbol, id) in &resources.symbols {
        let (kind, name) = symbol.split_once('/').context("a symbol is type/name")?;
        let field = name.replace('.', "_");
        let valid = field.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            bail!("{symbol}: {field} is not a Java identifier");
        }
        if let Some((_, fields)) = by_type.iter_mut().find(|(k, _)| *k == kind) {
            fields.push((field, *id));
        }
    }
    let mut out = format!(
        "// Generated by `android-res r-class` from android/res. Do not edit.\n\npackage {};\n\npublic final class R {{\n    private R() {{}}\n",
        args.package
    );
    for (kind, mut fields) in by_type.into_iter().filter(|(_, fields)| !fields.is_empty()) {
        fields.sort();
        out.push_str(&format!("\n    public static final class {kind} {{\n        private {kind}() {{}}\n\n"));
        for (field, id) in fields {
            out.push_str(&format!("        public static final int {field} = {id:#010x};\n"));
        }
        out.push_str("    }\n");
    }
    out.push_str("}\n");
    let dir = args.package.split('.').fold(args.out.clone(), |path, part| path.join(part));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("R.java");
    std::fs::write(&path, out)?;
    println!("{} ids -> {}", resources.symbols.len(), path.display());
    Ok(())
}

/// Every file under `dir`, with the manifest first and the table early, since a reader that
/// streams the zip meets them in this order.
fn staged(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&current)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    let rank = |path: &PathBuf| match path.file_name().and_then(|n| n.to_str()) {
        Some("AndroidManifest.xml") => 0,
        Some(apk::ARSC_NAME) => 1,
        _ => 2,
    };
    files.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    Ok(files)
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Compile(args) => run(&args)?,
        Command::Package(args) => {
            let files = staged(&args.dir)?;
            apk::write(&args.out, &args.dir, &files)?;
            println!("{} entries -> {}", files.len(), args.out.display());
        }
        Command::GenTable(args) => gen_table(&args)?,
        Command::RClass(args) => r_class(&args)?,
        Command::Vectors(args) => vector::write_all(&args.icons, &args.out)?,
    }
    Ok(())
}
