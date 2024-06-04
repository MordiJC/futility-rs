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
    writer: impl Write,
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
        (v.offset, usize::MAX - v.size, v.name.clone())
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
        if node.borrow().children.is_empty() {
            // Node with no children should have been already processed.
            continue;
        }
        // Special case for orphan nodes.
        if node.borrow().parent.is_none() {
            all_nodes.push(node.clone());
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
        (v.offset, usize::MAX - v.size, v.name.clone())
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
            &"".to_string(),
        )?;
        for alias in node.borrow().aliases.iter() {
            show_line(
                node_level,
                alias,
                node_offset,
                node_end,
                node_size,
                &mut writer,
                &"  // DUPLICATE".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    fn example_complex_fmap() -> fmap::FMap {
        fmap::FMap {
            name: "FLASH".to_string(),
            version_major: 1,
            version_minor: 1,
            base: 0,
            size: 33554432,
            areas: vec![
                fmap::FMapArea {
                    name: "SI_ALL".to_string(),
                    offset: 0,
                    size: 5242880,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "SI_DESC".to_string(),
                    offset: 0,
                    size: 4096,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "SI_ME".to_string(),
                    offset: 4096,
                    size: 5238784,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "CSE_LAYOUT".to_string(),
                    offset: 4096,
                    size: 8192,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "CSE_RO".to_string(),
                    offset: 12288,
                    size: 1679360,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "CSE_DATA".to_string(),
                    offset: 1691648,
                    size: 430080,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "CSE_RW".to_string(),
                    offset: 2121728,
                    size: 3080192,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "SI_BIOS".to_string(),
                    offset: 5242880,
                    size: 28311552,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "RW_SECTION_A".to_string(),
                    offset: 5242880,
                    size: 8388608,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "VBLOCK_A".to_string(),
                    offset: 5242880,
                    size: 65536,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "FW_MAIN_A".to_string(),
                    offset: 5308416,
                    size: 8323008,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "RW_FWID_A".to_string(),
                    offset: 13631424,
                    size: 64,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "RW_LEGACY".to_string(),
                    offset: 13631488,
                    size: 2097152,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "RW_MISC".to_string(),
                    offset: 15728640,
                    size: 1048576,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "UNIFIED_MRC_CACHE".to_string(),
                    offset: 15728640,
                    size: 131072,
                    flags: fmap::FMapFlags::Preserve,
                },
                fmap::FMapArea {
                    name: "RECOVERY_MRC_CACHE".to_string(),
                    offset: 15728640,
                    size: 65536,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "RW_MRC_CACHE".to_string(),
                    offset: 15794176,
                    size: 65536,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "RW_ELOG".to_string(),
                    offset: 15859712,
                    size: 16384,
                    flags: fmap::FMapFlags::Preserve,
                },
                fmap::FMapArea {
                    name: "RW_SHARED".to_string(),
                    offset: 15876096,
                    size: 16384,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "SHARED_DATA".to_string(),
                    offset: 15876096,
                    size: 8192,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "SHARED_DATA_DUPLICATE".to_string(),
                    offset: 15876096,
                    size: 8192,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "VBLOCK_DEV".to_string(),
                    offset: 15884288,
                    size: 8192,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "RW_SPD_CACHE".to_string(),
                    offset: 15892480,
                    size: 4096,
                    flags: fmap::FMapFlags::Preserve,
                },
                fmap::FMapArea {
                    name: "RW_VPD".to_string(),
                    offset: 15896576,
                    size: 8192,
                    flags: fmap::FMapFlags::Preserve,
                },
                fmap::FMapArea {
                    name: "RW_NVRAM".to_string(),
                    offset: 15904768,
                    size: 24576,
                    flags: fmap::FMapFlags::Preserve,
                },
                fmap::FMapArea {
                    name: "RW_SECTION_B".to_string(),
                    offset: 16777216,
                    size: 8388608,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "VBLOCK_B".to_string(),
                    offset: 16777216,
                    size: 65536,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "FW_MAIN_B".to_string(),
                    offset: 16842752,
                    size: 8323008,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "RW_FWID_B".to_string(),
                    offset: 25165760,
                    size: 64,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "WP_RO".to_string(),
                    offset: 25165824,
                    size: 8388608,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "RO_VPD".to_string(),
                    offset: 25165824,
                    size: 16384,
                    flags: fmap::FMapFlags::Preserve,
                },
                fmap::FMapArea {
                    name: "RO_SECTION".to_string(),
                    offset: 25182208,
                    size: 8372224,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "FMAP".to_string(),
                    offset: 25182208,
                    size: 2048,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "RO_FRID".to_string(),
                    offset: 25184256,
                    size: 64,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "GBB".to_string(),
                    offset: 25186304,
                    size: 458752,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "COREBOOT".to_string(),
                    offset: 25645056,
                    size: 7909376,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "COREBOOT_OVERLAP".to_string(),
                    offset: 25645057,
                    size: 7909377,
                    flags: fmap::FMapFlags::empty(),
                },
            ],
        }
    }

    #[test]
    fn test_dump_human_readable() -> Result<(), String> {
        init();
        let mut result = Vec::new();
        if let Err(e) = dump_human_readable(&example_complex_fmap(), false, true, &mut result) {
            return Err(format!("dump_human_readable() failed with error: {e}"));
        }
        let expected = r#"# name                     start       end         size
-entire flash-             00000000    02000000    02000000
  SI_ALL                     00000000    00500000    00500000
    SI_DESC                    00000000    00001000    00001000
    SI_ME                      00001000    00500000    004ff000
      CSE_LAYOUT                 00001000    00003000    00002000
      CSE_RO                     00003000    0019d000    0019a000
      CSE_DATA                   0019d000    00206000    00069000
      CSE_RW                     00206000    004f6000    002f0000
  SI_BIOS                    00500000    02000000    01b00000
    RW_SECTION_A               00500000    00d00000    00800000
      VBLOCK_A                   00500000    00510000    00010000
      FW_MAIN_A                  00510000    00cfffc0    007effc0
      RW_FWID_A                  00cfffc0    00d00000    00000040
    RW_LEGACY                  00d00000    00f00000    00200000
    RW_MISC                    00f00000    01000000    00100000
      UNIFIED_MRC_CACHE          00f00000    00f20000    00020000
        RECOVERY_MRC_CACHE         00f00000    00f10000    00010000
        RW_MRC_CACHE               00f10000    00f20000    00010000
      RW_ELOG                    00f20000    00f24000    00004000
      RW_SHARED                  00f24000    00f28000    00004000
        SHARED_DATA                00f24000    00f26000    00002000
        SHARED_DATA_DUPLICATE      00f24000    00f26000    00002000  // DUPLICATE
        VBLOCK_DEV                 00f26000    00f28000    00002000
      RW_SPD_CACHE               00f28000    00f29000    00001000
      RW_VPD                     00f29000    00f2b000    00002000
      RW_NVRAM                   00f2b000    00f31000    00006000
    RW_SECTION_B               01000000    01800000    00800000
      VBLOCK_B                   01000000    01010000    00010000
      FW_MAIN_B                  01010000    017fffc0    007effc0
      RW_FWID_B                  017fffc0    01800000    00000040
    WP_RO                      01800000    02000000    00800000
      RO_VPD                     01800000    01804000    00004000
      RO_SECTION                 01804000    02000000    007fc000
        FMAP                       01804000    01804800    00000800
        RO_FRID                    01804800    01804840    00000040
        GBB                        01805000    01875000    00070000
        COREBOOT                   01875000    02000000    0078b000
"#;
        assert_eq!(String::from_utf8(result).unwrap(), expected);

        Ok(())
    }

    #[test]
    fn test_dump_human_readable_with_gaps() -> Result<(), String> {
        init();
        let mut result = Vec::new();
        if let Err(e) = dump_human_readable(&example_complex_fmap(), true, true, &mut result) {
            return Err(format!("dump_human_readable() failed with error: {e}"));
        }
        let expected = r#"# name                     start       end         size
-entire flash-             00000000    02000000    02000000
  SI_ALL                     00000000    00500000    00500000
    SI_DESC                    00000000    00001000    00001000
    SI_ME                      00001000    00500000    004ff000
      CSE_LAYOUT                 00001000    00003000    00002000
      CSE_RO                     00003000    0019d000    0019a000
      CSE_DATA                   0019d000    00206000    00069000
      CSE_RW                     00206000    004f6000    002f0000
  SI_BIOS                    00500000    02000000    01b00000
    RW_SECTION_A               00500000    00d00000    00800000
      VBLOCK_A                   00500000    00510000    00010000
      FW_MAIN_A                  00510000    00cfffc0    007effc0
      RW_FWID_A                  00cfffc0    00d00000    00000040
    RW_LEGACY                  00d00000    00f00000    00200000
    RW_MISC                    00f00000    01000000    00100000
      UNIFIED_MRC_CACHE          00f00000    00f20000    00020000
        RECOVERY_MRC_CACHE         00f00000    00f10000    00010000
        RW_MRC_CACHE               00f10000    00f20000    00010000
      RW_ELOG                    00f20000    00f24000    00004000
      RW_SHARED                  00f24000    00f28000    00004000
        SHARED_DATA                00f24000    00f26000    00002000
        SHARED_DATA_DUPLICATE      00f24000    00f26000    00002000  // DUPLICATE
        VBLOCK_DEV                 00f26000    00f28000    00002000
      RW_SPD_CACHE               00f28000    00f29000    00001000
      RW_VPD                     00f29000    00f2b000    00002000
      RW_NVRAM                   00f2b000    00f31000    00006000
    RW_SECTION_B               01000000    01800000    00800000
      VBLOCK_B                   01000000    01010000    00010000
      FW_MAIN_B                  01010000    017fffc0    007effc0
      RW_FWID_B                  017fffc0    01800000    00000040
    WP_RO                      01800000    02000000    00800000
      RO_VPD                     01800000    01804000    00004000
      RO_SECTION                 01804000    02000000    007fc000
        FMAP                       01804000    01804800    00000800
        RO_FRID                    01804800    01804840    00000040
        [UNUSED]                   01804840    01805000    000007c0
        GBB                        01805000    01875000    00070000
        COREBOOT                   01875000    02000000    0078b000
"#;
        assert_eq!(String::from_utf8(result).unwrap(), expected);

        Ok(())
    }

    #[test]
    fn test_dump_humap_readable_do_not_ignore_overlaps() -> Result<(), String> {
        init();
        let mut result = Vec::new();
        if dump_human_readable(&example_complex_fmap(), true, false, &mut result).is_ok() {
            Err("Overlap error expected, got Ok()".to_string())
        } else {
            Ok(())
        }
    }

    fn example_fmap() -> fmap::FMap {
        fmap::FMap {
            name: "example".to_string(),
            base: 0,
            size: 0x400,
            version_major: 1,
            version_minor: 1,
            areas: vec![
                fmap::FMapArea {
                    name: "bootblock".to_string(),
                    offset: 0,
                    size: 0x80,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "normal".to_string(),
                    offset: 0x80,
                    size: 0x80,
                    flags: fmap::FMapFlags::Preserve,
                },
                fmap::FMapArea {
                    name: "fallback".to_string(),
                    offset: 0x100,
                    size: 0x100,
                    flags: fmap::FMapFlags::empty(),
                },
                fmap::FMapArea {
                    name: "data".to_string(),
                    offset: 0x200,
                    size: 0x200,
                    flags: fmap::FMapFlags::empty(),
                },
            ],
        }
    }

    #[test]
    fn test_dump_parsable() -> Result<(), String> {
        let mut result = Vec::new();
        if let Err(e) = dump_parsable(&example_fmap(), &mut result) {
            return Err(format!("dump_parsable() failed with error: {e}"));
        }
        let expected = "bootblock 0 128\n\
                        normal 128 128\n\
                        fallback 256 256\n\
                        data 512 512\n";
        assert_eq!(String::from_utf8(result).unwrap(), expected);

        Ok(())
    }

    #[test]
    fn test_dump_flashrom_parsable() -> Result<(), String> {
        let mut result = Vec::new();
        if let Err(e) = dump_flashrom_parsable(&example_fmap(), &mut result) {
            return Err(format!("dump_flashrom_parsable() failed with error: {e}"));
        }
        let expected = "0x000000:0x00007f bootblock\n\
                        0x000080:0x0000ff normal\n\
                        0x000100:0x0001ff fallback\n\
                        0x000200:0x0003ff data\n";
        assert_eq!(String::from_utf8(result).unwrap(), expected);

        Ok(())
    }

    #[test]
    fn test_dump_ec_parsable() -> Result<(), String> {
        let mut result = Vec::new();
        if let Err(e) = dump_ec_parsable(&example_fmap(), &mut result) {
            return Err(format!("dump_ec_parsable() failed with error: {e}"));
        }
        let expected = "bootblock 0 128 not-preserve\n\
                        normal 128 128 preserve\n\
                        fallback 256 256 not-preserve\n\
                        data 512 512 not-preserve\n";
        assert_eq!(String::from_utf8(result).unwrap(), expected);

        Ok(())
    }
}
