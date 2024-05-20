use std::error::Error;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};

use camino::Utf8PathBuf;
use clap::{Args, ValueHint};

use crate::{cmd::common, fmap};

#[derive(Args)]
pub struct ExtractFmapArgs {
    #[arg(required = true, index = 1, value_hint = ValueHint::FilePath, value_parser = common::file_exists_validator)]
    /// Firmware image path.
    pub(in crate::cmd) image: Utf8PathBuf,

    #[arg(required = true, index = 2, trailing_var_arg = true, value_parser = extraction_param_valid)]
    /// List of mappings from FlashMap section to file in format SECTION:FILE.
    /// Example: FW_MAIN_A:fw_main_a.bin
    pub(in crate::cmd) params: Vec<(String, Utf8PathBuf)>,
}

pub(in crate::cmd) fn extraction_param_valid(s: &str) -> Result<(String, Utf8PathBuf), String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return Err(String::from(
            "The argument should be in the format 'SECTION:PATH'",
        ));
    }
    Ok((String::from(parts[0]), Utf8PathBuf::from(parts[1])))
}

pub fn run_command(args: &ExtractFmapArgs) -> Result<(), Box<dyn Error>> {
    let mut input_file = File::open(&args.image)?;
    let (fmap, _) = fmap::FMap::find_fmap(&input_file)?;
    let mut errors_encountered = false;

    for (area_name, output_path) in args.params.iter() {
        let aro = fmap.get(area_name);
        if aro.is_none() {
            eprintln!("FlashMap area '{}' not found", area_name);
            errors_encountered = true;
            continue;
        }

        // Verify area
        let ar = aro.unwrap();
        if ar.offset == 0 {
            eprintln!("Area '{}' has zero size", area_name);
            continue;
        }
        if ar.offset + ar.size > fmap.size {
            eprintln!("Area '{}' stretches beyond image", area_name);
            continue;
        }

        if let Err(error) = input_file.seek(SeekFrom::Start(ar.offset as u64)) {
            eprintln!(
                "Unable to read from image file '{}' at {}. Error: {:?}",
                args.image, ar.offset, error
            );
        }

        let mut area_buf: Vec<u8> = vec![0u8; ar.size as usize];
        if let Err(error) = input_file.read_exact(&mut area_buf) {
            eprintln!(
                "Unable to read from image file '{}'. Error: {:?}",
                args.image, error
            );
        }

        if let Err(error) = fs::write(output_path, area_buf) {
            eprintln!(
                "Unable to write to the file '{}'. Error: {:?}",
                output_path, error
            );
        }
    }

    if errors_encountered {
        Err("Errors occured during extraction. Data might not be valid.".into())
    } else {
        Ok(())
    }
}
