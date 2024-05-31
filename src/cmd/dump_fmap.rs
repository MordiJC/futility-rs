use std::cell::RefCell;
use std::error::Error;
use std::fs::File;
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

    #[arg(long, short = 'H', action, requires = "human_readable")]
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
    pub children: Vec<Rc<RefCell<Node>>>,
    pub valid: bool,
}

fn dump_human_readable(fmap: &fmap::FMap, show_gaps: bool, ignore_overlap: bool) {
    // Sort ascending by offset and descending by size to push larger areas first.
    let sorted = fmap
        .areas
        .iter()
        .sorted_unstable_by_key(|a| (a.offset, u32::MAX - a.size))
        .collect::<Vec<_>>();

    // Convert into nodes.
    let nodes = &mut sorted
        .iter()
        .map(|ar| {
            Rc::new(RefCell::new(Node {
                name: ar.name.clone(),
                offset: ar.offset as usize,
                size: ar.size as usize,
                aliases: vec![],
                children: vec![],
                valid: true,
            }))
        })
        .collect_vec();
    nodes.push(Rc::new(RefCell::new(Node {
        name: String::from("-entire flash-"),
        offset: fmap.base as usize,
        size: fmap.size as usize,
        aliases: vec![],
        children: vec![],
        valid: true,
    })));

    // Transform into graph/tree.
    let mut overlaps = 0;
    for i in 0..(nodes.len() - 1) {
        let mut smallest_index = nodes.len() - 1; // Root node.
        let ara_name = nodes[i].borrow().name.clone();
        let ara_offset = nodes[i].borrow().offset;
        let ara_size = nodes[i].borrow().size;
        let ara_end = ara_offset + ara_size;

        for j in 0..nodes.len() {
            if i == j || !nodes[j].borrow().valid {
                continue;
            }

            let arb_name = nodes[j].borrow().name.clone();
            let arb_offset = nodes[j].borrow().offset;
            let arb_size = nodes[j].borrow().size;
            let arb_end = arb_offset + arb_size;

            // Check for overlap
            if ara_offset < arb_offset && arb_offset < ara_end && ara_end < arb_end {
                if ignore_overlap {
                    error!(
                        r#"ERROR: Areas "{}" ({:#x} - {:#x}) and "{}" ({:#x} - {:#x}) overlap!"#,
                        ara_name, ara_offset, ara_end, arb_name, arb_offset, arb_end
                    );
                    overlaps += 1;
                }
                continue;
            }

            // Check for duplicates. Invalidate dupliacete nodes.
            if ara_offset == arb_offset && ara_size == arb_size {
                nodes[j].borrow_mut().valid = false;
                nodes[i].borrow_mut().aliases.push(arb_name);
                continue;
            }

            if arb_offset <= ara_offset
                && arb_end >= ara_end
                && arb_size < nodes[smallest_index].borrow().size
            {
                smallest_index = j;
            }
        }

        nodes[smallest_index]
            .borrow_mut()
            .children
            .push(Rc::clone(&nodes[i]));
    }
    if overlaps != 0 {
        return;
    }

    println!("# name                     start       end         size");
    let mut gap_count = 0;
    show(
        &nodes[nodes.len() - 1].borrow(),
        0,
        show_gaps,
        &mut gap_count,
    );

    if gap_count > 0 {
        warn!("WARNING: Gaps in FlashMap found. Use -H to show them.");
    }
}

fn show(node: &Node, level: usize, show_gaps: bool, gap_count: &mut i32) {
    show_node(&node.name, node.offset, node.size, level, false);
    for alias in node.aliases.iter() {
        show_node(alias, node.offset, node.size, level + 1, false);
    }

    let node_end = node.offset + node.size;
    for (i, child) in node.children.iter().enumerate() {
        let child_end = child.borrow().offset + child.borrow().size;
        if i == 0 && node.offset != node.children[i].borrow().offset {
            if show_gaps {
                show_node(
                    &node.name,
                    child.borrow().offset,
                    node.children[i].borrow().offset - child.borrow().offset,
                    level,
                    true,
                );
            }
            *gap_count += 1;
        }

        show(&child.borrow(), level + 1, show_gaps, gap_count);

        if i < node.children.len() - 1 && child_end != node.children[i + 1].borrow().offset {
            if show_gaps {
                show_node(
                    &node.name,
                    child_end,
                    node.children[i + 1].borrow().offset - child_end,
                    level + 1,
                    true,
                );
            }
            *gap_count += 1;
        }
        if i == node.children.len() - 1 && node_end != child_end {
            if show_gaps {
                show_node(&node.name, child_end, node_end - child_end, level + 1, true);
            }
            *gap_count += 1;
        }
    }
}

fn show_node(name: &String, offset: usize, size: usize, level: usize, gap: bool) {
    let empty_string = String::from("");
    println!(
        "{}{: <25}  {:08x}    {:08x}    {:08x}{}",
        "  ".repeat(level),
        if !gap { name } else { &empty_string },
        offset,
        offset + size,
        size,
        if gap {
            format!("  //gap in {}", name)
        } else {
            "".to_string()
        }
    );
}

fn dump_default(fmap: &fmap::FMap, offset: usize) {
    println!("hit at {offset:#x}");
    println!("fmap_signature:  __FMAP__"); // Original futility has no colon here
    println!(
        "fmap_version:    {}.{}",
        fmap.version_major, fmap.version_minor
    );
    println!("fmap_base:       {:#x}", fmap.base);
    println!("fmap_size:       {0:#x} ({0})", fmap.size);
    println!("fmap_name:       {}", fmap.name);
    println!("fmap_nareas:     {}", fmap.areas.len());
    for (i, area) in fmap.areas.iter().enumerate() {
        println!("area:            {}", i + 1);
        println!("area_offset:     {:#x}", area.offset);
        println!("area_size:       {0:#x} ({0})", area.size);
        println!("area_name:       {}", area.name);
    }
}

fn dump_parsable(fmap: &fmap::FMap) {
    for area in fmap.areas.iter() {
        println!("{} {} {}", area.name, area.offset, area.size);
    }
}

fn dump_flashrom_parsable(fmap: &fmap::FMap) {
    for area in fmap.areas.iter() {
        println!(
            "{:#08x}:{:#08x} {}",
            area.offset,
            (area.offset + area.size - 1),
            area.name
        );
    }
}

fn dump_ec_parsable(fmap: &fmap::FMap) {
    for area in fmap.areas.iter() {
        println!(
            "{} {} {} {}",
            area.name,
            area.offset,
            area.size,
            if area.flags.contains(fmap::FMapFlags::Preserve) {
                "preserve"
            } else {
                "not-preserve"
            }
        );
    }
}

pub fn run_command(args: &DumpFmapArgs) -> Result<(), Box<dyn Error>> {
    if args.extract {
        let extract_args = extract_fmap::ExtractFmapArgs {
            image: args.image.clone(),
            params: args.params.clone(),
        };
        return extract_fmap::run_command(&extract_args);
    }

    let input_file = File::open(&args.image)?;
    let (fmap, fmap_offset) = fmap::FMap::find_fmap(&input_file)?;

    if args.human_readable {
        dump_human_readable(
            &fmap,
            args.human_readable_with_gaps,
            args.ignore_overlapping_sections,
        );
    } else if args.parsable {
        dump_parsable(&fmap);
    } else if args.flashrom_parsable {
        dump_flashrom_parsable(&fmap);
    } else if args.ec_parsable {
        dump_ec_parsable(&fmap);
    } else {
        dump_default(&fmap, fmap_offset);
    }

    Ok(())
}
