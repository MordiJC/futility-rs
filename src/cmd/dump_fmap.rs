use std::cell::RefCell;
use std::error::Error;
use std::fs::File;
use std::io::{stdout, Write};
use std::rc::Rc;

use camino::Utf8PathBuf;
use clap::builder::ArgPredicate;
use clap::{ArgAction, Args, ValueHint};
use itertools::Itertools;
use log::{error, warn};

use crate::{
    cmd::{common, extract_fmap},
    fmap,
};

#[derive(Args)]
pub struct DumpFmapArgs {
    #[arg(index = 1, value_hint = ValueHint::FilePath)]
    /// Firmware image path.
    image: Utf8PathBuf,

    #[arg(long, short = 'x', action, hide = true,
          conflicts_with_all = ["human_readable", "parsable", "flashrom_parsable", "ec_parsable"])]
    /// Extract sections to the file.
    extract: bool,

    #[arg(long, short = 'h', action,
        conflicts_with_all = ["extract", "parsable", "flashrom_parsable", "ec_parsable"],
        default_value_if("human_readable_with_gaps", ArgPredicate::IsPresent, Some("true"))
    )]
    /// Display using human-readable format.
    human_readable: bool,

    #[arg(long, short = 'H', action)]
    /// Include gaps in human-readable format. Implies human-readable format.
    human_readable_with_gaps: bool,

    #[arg(long, action, requires = "human_readable")]
    /// Do not report nor terminate on encountering overlapping sections.
    ignore_overlapping_sections: bool,

    #[arg(long, short, action,
          conflicts_with_all = ["extract", "human_readable", "flashrom_parsable", "ec_parsable"])]
    /// Use format easy to parse by scripts.
    /// <area> <offset> <size>
    parsable: bool,

    #[arg(long, short = 'F', action,
          conflicts_with_all = ["extract", "human_readable", "parsable", "ec_parsable"])]
    /// Use format expected by flashrom.
    flashrom_parsable: bool,

    #[arg(long, short, action,
          conflicts_with_all = ["extract", "human_readable", "parsable", "flashrom_parsable"])]
    /// Use format expected by flash_ec.
    ec_parsable: bool,

    #[arg(long, action = ArgAction::Help)]
    /// Print help.
    help: Option<bool>,

    #[arg(index = 2, trailing_var_arg = true, value_parser = common::area_to_file_mapping_param_valid, hide = true)]
    params: Vec<(String, Utf8PathBuf)>,
}

#[derive(Debug)]
struct Node {
    pub name: String,
    pub offset: usize,
    pub size: usize,
    pub aliases: Vec<String>,
    pub parent: Option<Rc<RefCell<Node>>>,
    pub children: Vec<Rc<RefCell<Node>>>,
}

impl Node {
    pub fn is_duplicate(&self, node: &Node) -> bool {
        self.offset == node.offset && self.size == node.size
    }

    pub fn end(&self) -> usize {
        self.offset + self.size
    }

    pub fn overlaps(&self, node: &Node) -> bool {
        (self.offset < node.offset && node.offset < self.end() && self.end() < node.end())
            || (node.offset < self.offset && self.offset < node.end() && node.end() < self.end())
    }

    pub fn fits_in(&self, node: &Node) -> bool {
        self.offset >= node.offset && self.end() <= node.end()
    }

    pub fn parents_number(&self) -> usize {
        match &self.parent {
            None => 0,
            Some(p) => p.borrow().parents_number() + 1,
        }
    }
}

impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.is_duplicate(other)
    }
}

fn dump_human_readable(
    fmap: &fmap::FMap,
    show_gaps: bool,
    ignore_overlap: bool,
    mut writer: impl Write,
) -> Result<(), Box<dyn Error>> {
    // Convert into nodes.
    let mut nodes = fmap
        .areas
        .iter()
        .map(|ar| {
            Rc::new(RefCell::new(Node {
                name: ar.name.clone(),
                offset: ar.offset as usize,
                size: ar.size as usize,
                aliases: vec![],
                parent: None,
                children: vec![],
            }))
        })
        .collect::<Vec<_>>();
    nodes.push(Rc::new(RefCell::new(Node {
        name: String::from("-entire flash-"),
        offset: fmap.base as usize,
        size: fmap.size as usize,
        aliases: vec![],
        parent: None,
        children: vec![],
    })));

    // Sort ascending by offset and descending by size to push larger areas first.
    nodes.sort_unstable_by_key(|a| {
        let v = a.borrow();
        (v.offset, usize::MAX - v.size)
    });

    // Remove duplicates and find overlaps
    let mut deduplicated = vec![nodes[0].clone()];
    let mut overlaps = 0;
    'dedup_outer: for node in nodes.iter().skip(1) {
        for d in deduplicated.iter() {
            if node.borrow().is_duplicate(&d.borrow_mut()) {
                d.borrow_mut().aliases.push(node.borrow().name.clone());
                continue 'dedup_outer;
            } else if node.borrow().overlaps(&d.borrow()) {
                error!(
                    r#"Areas "{}" ({:#x} - {:#x}) and "{}" ({:#x} - {:#x}) overlap!"#,
                    d.borrow().name,
                    d.borrow().offset,
                    d.borrow().end(),
                    node.borrow().name,
                    node.borrow().offset,
                    node.borrow().end()
                );
                if !ignore_overlap {
                    overlaps += 1;
                }
                continue 'dedup_outer;
            }
        }
        // Add first occurrence of entry.
        deduplicated.push(node.clone());
    }
    drop(nodes);

    if overlaps != 0 {
        return Err(format!("{overlaps} overlapping areas detected. Terminating.").into());
    }

    // Skip first as it will (or at leas should) be the root node.
    for i in 1..deduplicated.len() {
        let mut node_a = deduplicated[i].borrow_mut();
        for k in (0..i).rev() {
            let mut node_b = deduplicated[k].borrow_mut();
            if node_a.fits_in(&node_b) {
                node_a.parent = Some(deduplicated[k].clone());
                node_b.children.push(deduplicated[i].clone());
                break;
            }
        }
    }

    // Check for gaps.
    let mut gap_count = 0;
    let mut all_nodes = Vec::<Rc<RefCell<Node>>>::new();
    for node in deduplicated.iter() {
        if node.borrow().parent.is_none() {
            // Node with no parent do not need any processing.
            all_nodes.push(node.clone());
            continue;
        } else if node.borrow().children.is_empty() {
            // Node with no children should have been already processed.
            continue;
        }

        let node_offset = node.borrow().offset;
        let node_end = node.borrow().end();

        // Create new list of children to easily insert gap entries if necessary.
        let mut new_children = Vec::<Rc<RefCell<Node>>>::new();
        for i in 0..node.borrow().children.len() {
            let child_offset = node.borrow().children[i].borrow().offset;
            let child_end = node.borrow().children[i].borrow().end();

            // First child. Check with parent.
            if i == 0 && node_offset < child_offset {
                gap_count += 1;
                if show_gaps {
                    new_children.push(Rc::new(RefCell::new(Node {
                        name: "[UNUSED]".to_string(),
                        offset: node_offset,
                        size: child_offset - node_offset,
                        aliases: vec![],
                        parent: Some(node.clone()),
                        children: vec![],
                    })));
                }
            } else if i != 0 {
                // Non-first child. Check with previous child.
                let left_child_end = node.borrow().children[i - 1].borrow().end();

                if left_child_end < child_offset {
                    gap_count += 1;
                    if show_gaps {
                        new_children.push(Rc::new(RefCell::new(Node {
                            name: "[UNUSED]".to_string(),
                            offset: left_child_end,
                            size: child_offset - left_child_end,
                            aliases: vec![],
                            parent: Some(node.clone()),
                            children: vec![],
                        })));
                    }
                }
            }

            // Move current child.
            new_children.push(node.borrow().children[i].clone());

            // Handle last child in similar manner as first child.
            if i == node.borrow().children.len() && node_end > child_end {
                gap_count += 1;
                if show_gaps {
                    new_children.push(Rc::new(RefCell::new(Node {
                        name: "[UNUSED]".to_string(),
                        offset: node_end,
                        size: node_end - child_end,
                        aliases: vec![],
                        parent: Some(node.clone()),
                        children: vec![],
                    })));
                }
            }
        }

        // Replace node children with another one.
        // NOTE: Maybe unnecessary at this point, but let's keep structure valid from both ends.
        node.borrow_mut().children.clear();
        node.borrow_mut().children.append(&mut new_children);

        all_nodes.append(&mut node.borrow().children.iter().cloned().collect_vec());
    }

    drop(deduplicated);

    all_nodes.sort_unstable_by_key(|a| {
        let v = a.borrow();
        (v.offset, usize::MAX - v.size)
    });

    show(&all_nodes, writer)?;

    if !show_gaps && gap_count > 0 {
        warn!("WARNING: Gaps in FlashMap found. Use -H to show them.");
    }
    Ok(())
}

fn show(nodes: &[Rc<RefCell<Node>>], mut writer: impl Write) -> Result<(), Box<dyn Error>> {
    writeln!(
        writer,
        "# name                     start       end         size"
    )?;
    for node in nodes.iter() {
        let (node_level, node_name, node_offset, node_end, node_size) = {
            let n = node.borrow();
            (
                n.parents_number(),
                n.name.clone(),
                n.offset,
                n.end(),
                n.size,
            )
        };
        show_line(
            node_level,
            &node_name,
            node_offset,
            node_end,
            node_size,
            &mut writer,
            &"".into(),
        )?;
        for alias in node.borrow().aliases.iter() {
            show_line(
                node_level,
                alias,
                node_offset,
                node_end,
                node_size,
                &mut writer,
                &"  // DUPLICATE".into(),
            )?;
        }
    }
    Ok(())
}

fn show_line(
    level: usize,
    name: &String,
    offset: usize,
    end: usize,
    size: usize,
    mut writer: impl Write,
    suffix: &String,
) -> Result<(), Box<dyn Error>> {
    match writeln!(
        writer,
        "{}{: <25}  {:08x}    {:08x}    {:08x}{}",
        "  ".repeat(level),
        name,
        offset,
        end,
        size,
        suffix
    ) {
        Ok(()) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn dump_default(fmap: &fmap::FMap, offset: usize, mut writer: impl Write) -> std::io::Result<()> {
    writeln!(writer, "hit at {offset:#x}")?;
    writeln!(writer, "fmap_signature:  __FMAP__")?; // Original futility has no colon here
    writeln!(
        writer,
        "fmap_version:    {}.{}",
        fmap.version_major, fmap.version_minor
    )?;
    writeln!(writer, "fmap_base:       {:#x}", fmap.base)?;
    writeln!(writer, "fmap_size:       {0:#x} ({0})", fmap.size)?;
    writeln!(writer, "fmap_name:       {}", fmap.name)?;
    writeln!(writer, "fmap_nareas:     {}", fmap.areas.len())?;
    for (i, area) in fmap.areas.iter().enumerate() {
        writeln!(writer, "area:            {}", i + 1)?;
        writeln!(writer, "area_offset:     {:#x}", area.offset)?;
        writeln!(writer, "area_size:       {0:#x} ({0})", area.size)?;
        writeln!(writer, "area_name:       {}", area.name)?;
    }
    Ok(())
}

fn dump_parsable(fmap: &fmap::FMap, mut writer: impl Write) -> std::io::Result<()> {
    for area in fmap.areas.iter() {
        writeln!(writer, "{} {} {}", area.name, area.offset, area.size)?;
    }
    Ok(())
}

fn dump_flashrom_parsable(fmap: &fmap::FMap, mut writer: impl Write) -> std::io::Result<()> {
    for area in fmap.areas.iter() {
        writeln!(
            writer,
            "{:#08x}:{:#08x} {}",
            area.offset,
            (area.offset + area.size - 1),
            area.name
        )?;
    }
    Ok(())
}

fn dump_ec_parsable(fmap: &fmap::FMap, mut writer: impl Write) -> std::io::Result<()> {
    for area in fmap.areas.iter() {
        writeln!(
            writer,
            "{} {} {} {}",
            area.name,
            area.offset,
            area.size,
            if area.flags.contains(fmap::FMapFlags::Preserve) {
                "preserve"
            } else {
                "not-preserve"
            }
        )?;
    }
    Ok(())
}

pub fn run_command(args: &DumpFmapArgs) -> Result<(), Box<dyn Error>> {
    if args.extract {
        let extract_args = extract_fmap::ExtractFmapArgs {
            image: args.image.clone(),
            params: args.params.clone(),
        };
        return extract_fmap::run_command(&extract_args);
    }

    let mut input_file = File::open(&args.image)?;
    let (fmap, fmap_offset) = fmap::FMap::find_fmap(&mut input_file)?;

    if args.human_readable {
        dump_human_readable(
            &fmap,
            args.human_readable_with_gaps,
            args.ignore_overlapping_sections,
            &mut stdout(),
        )?;
    } else if args.parsable {
        dump_parsable(&fmap, &mut stdout())?;
    } else if args.flashrom_parsable {
        dump_flashrom_parsable(&fmap, &mut stdout())?;
    } else if args.ec_parsable {
        dump_ec_parsable(&fmap, &mut stdout())?;
    } else {
        dump_default(&fmap, fmap_offset, &mut stdout())?;
    }

    Ok(())
}
