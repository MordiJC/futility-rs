use std::error::Error;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::{cmd::common, fmap};
use camino::Utf8PathBuf;
use clap::{arg, Args, ValueHint};
use log::{error, info};
use tempfile::tempfile;

#[derive(Args)]
pub struct LoadFmapArgs {
    #[arg(required = true, index = 1, value_hint = ValueHint::FilePath, value_parser = common::file_exists_validator)]
    /// Firmware image path.
    pub(in crate::cmd) image: Utf8PathBuf,

    #[arg(required = true, index = 2, trailing_var_arg = true, value_parser = common::area_to_file_mapping_param_valid)]
    /// List of mappings from FlashMap section to file in format SECTION:FILE.
    /// Example: FW_MAIN_A:fw_main_a.bin
    pub(in crate::cmd) params: Vec<(String, Utf8PathBuf)>,

    #[arg(short, long, value_hint = ValueHint::FilePath)]
    /// Output file path.
    pub(in crate::cmd) output: Option<Utf8PathBuf>,

    #[arg(long, default_value = "0xff", value_parser = common::decimal_or_hex_validator_u8)]
    pub(in crate::cmd) fill_value: u8,
}

pub fn run_command(args: &LoadFmapArgs) -> Result<(), Box<dyn Error>> {
    let mut input_file = File::open(&args.image)?;
    let (fmap, _) = fmap::FMap::find_fmap(&mut input_file)?;

    let mut output_file = tempfile()?;
    if let Err(e) = std::io::copy(&mut input_file, &mut output_file) {
        return Err(format!("Failed to prepare workfile. Please check permissions to default temporary directory: `{}'. Error: {e}", std::env::temp_dir().display()).into());
    }

    let mut errors_encountered = false;
    for (area_name, path) in args.params.iter() {
        let ar = match fmap.get(area_name) {
            None => {
                error!("FlashMap area '{}' not found", area_name);
                errors_encountered = true;
                continue;
            }
            Some(v) => v,
        };

        // Verify area
        if ar.offset + ar.size > fmap.size {
            error!("Area '{}' stretches beyond image", area_name);
            errors_encountered = true;
            continue;
        }

        let mut area_file = match File::open(path) {
            Err(e) => {
                error!("Failed to open file `{path}'. Error: {e}");
                errors_encountered = true;
                continue;
            }
            Ok(v) => v,
        };

        let mut buf = vec![args.fill_value; ar.size as usize];
        match area_file.read(&mut buf) {
            Err(e) => {
                error!("Failed to read file `{path}': Error: {e}");
                errors_encountered = true;
                continue;
            }
            Ok(v) => {
                info!("Read {v} bytes from `{path}'");
            }
        };

        if let Err(e) = output_file.seek(SeekFrom::Start(ar.offset as u64)) {
            error!("Failed to write to the area '{area_name}', Error: {e}");
            errors_encountered = true;
            continue;
        }

        if let Err(e) = output_file.write(&buf) {
            error!("Failed to write to the area '{area_name}', Error: {e}");
            errors_encountered = true;
        }
    }

    if errors_encountered {
        return Err("Errors occured during loading".into());
    }
    match &args.output {
        Some(path) => {
            let mut final_file = match File::create(path) {
                Err(e) => {
                    return Err(format!(
                        "Failed to move data from workbuffer to the output file. Error: {e}"
                    )
                    .into());
                }
                Ok(f) => f,
            };
            if let Err(e) = std::io::copy(&mut output_file, &mut final_file) {
                return Err(format!(
                    "Failed to move data from workbuffer to the output file. Error: {e}"
                )
                .into());
            }
        }
        None => {
            if let Err(e) = std::io::copy(&mut output_file, &mut input_file) {
                return Err(format!(
                    "Failed to move data from workbuffer to the output file. Error: {e}"
                )
                .into());
            }
        }
    }
    Ok(())
}
