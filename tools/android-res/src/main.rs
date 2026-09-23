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
mod xml;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use arsc::{Entry, Res, Type};
use compile::Symbols;

/// Types in the order their ids are assigned, which is what `@type/name` resolves through.
const TYPES: [&str; 4] = ["color", "drawable", "mipmap", "style"];
const VALUES: &str = "values.xml";

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
    let values_text = std::fs::read_to_string(res.join(VALUES))
        .with_context(|| format!("reading {}", res.join(VALUES).display()))?;

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
    for node in parsed.root_element().children().filter(roxmltree::Node::is_element) {
        let name = node.attribute("name").context("a resource needs a name")?.to_owned();
        match node.tag_name().name() {
            "color" => colors.push((name, node.text().unwrap_or_default().trim().to_owned())),
            "style" => styles.push(node),
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
        },
    ];
    types.retain(|t| !t.entries.is_empty() || t.name == "color");
    Ok(Resources { types, symbols, files })
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
    let manifest = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
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
    for line in input.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("public final class android.R$") {
            class = rest.split_whitespace().next().unwrap_or_default().to_owned();
            continue;
        }
        let Some(rest) = line.strip_prefix("public static final int ") else {
            continue;
        };
        let Some((name, value)) = rest.trim_end_matches(';').split_once(" = ") else {
            continue;
        };
        let Ok(id) = value.trim().parse::<u32>() else {
            continue;
        };
        match class.as_str() {
            "attr" => attrs.push((name.trim().to_owned(), id)),
            "style" => styles.push((name.trim().to_owned(), id)),
            _ => {}
        }
    }
    attrs.sort_by(|a, b| a.0.cmp(&b.0));
    styles.sort_by(|a, b| a.0.cmp(&b.0));

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
        "pub fn lookup(table: &[(&str, u32)], name: &str) -> Option<u32> {\n    \
         table.binary_search_by(|(n, _)| (*n).cmp(name)).ok().map(|i| table[i].1)\n}\n",
    );
    std::fs::write(&args.out, out)?;
    println!("{} attrs, {} styles -> {}", attrs.len(), styles.len(), args.out);
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
    }
    Ok(())
}
