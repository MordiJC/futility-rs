use bitflags::bitflags;
use std::io::{Read, Seek, SeekFrom};
use std::mem;
use thiserror;

/* FMAP structs. See http://code.google.com/p/flashmap/wiki/FmapSpec */
bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct FMapFlags: u16 {
        const Static = 1 << 0;
        const Compressed = 1 << 1;
        const RO = 1 << 2;
        const Preserve = 1 << 3;
    }
}

pub const SEARCH_STRIDE: usize = 4;
pub const NAME_LEN: usize = 32;
pub const SIGNATURE: &[u8; 8] = b"__FMAP__";
pub const VERSION_MAJOR: u32 = 1;
pub const HEADER_SIZE: usize = SIGNATURE.len() + 1 + 1 + 8 + 4 + NAME_LEN + 2;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct FMapArea {
    pub name: String,
    pub offset: u32,
    pub size: u32,
    pub flags: FMapFlags,
}

#[derive(Debug, Default)]
#[repr(C, packed)]
struct FMapAreaRaw {
    offset: u32,
    size: u32,
    name: [u8; NAME_LEN],
    flags: u16,
}

impl From<FMapAreaRaw> for FMapArea {
    fn from(fmap_area_raw: FMapAreaRaw) -> FMapArea {
        let fmap_name: String = if fmap_area_raw.name.contains(&0_u8) {
            std::ffi::CStr::from_bytes_until_nul(&fmap_area_raw.name)
                .unwrap()
                .to_str()
                .unwrap_or("")
        } else {
            std::str::from_utf8(&fmap_area_raw.name).unwrap_or("")
        }
        .to_string();

        FMapArea {
            name: fmap_name,
            offset: fmap_area_raw.offset,
            size: fmap_area_raw.size,
            flags: FMapFlags::from_bits(fmap_area_raw.flags).unwrap_or(FMapFlags::empty()),
        }
    }
}

#[derive(Debug, Default)]
pub struct FMap {
    pub name: String,
    pub version_major: u8,
    pub version_minor: u8,
    pub base: u64,
    pub size: u32,
    pub areas: Vec<FMapArea>,
}

#[derive(Debug, Default)]
#[repr(C, packed)]
struct FMapRaw {
    signature: [u8; SIGNATURE.len()],
    version_major: u8,
    version_minor: u8,
    base: u64,
    size: u32,
    name: [u8; NAME_LEN],
    nareas: u16,
}

#[derive(thiserror::Error, Debug)]
pub enum FMapError {
    #[error("flash map not found")]
    NotFound,
    #[error("flash map header corrupted")]
    CorruptedHeader,
    #[error("incorrect or unsupported flash map version: {}.{}", .0, .1)]
    IncorrectVersion(u8, u8),
    #[error("io error")]
    IOError {
        #[from]
        source: std::io::Error,
    },
}

impl From<FMapRaw> for FMap {
    fn from(fmap_raw: FMapRaw) -> FMap {
        let fmap_name: String = if fmap_raw.name.contains(&0_u8) {
            std::ffi::CStr::from_bytes_until_nul(&fmap_raw.name)
                .unwrap()
                .to_str()
                .unwrap_or("")
        } else {
            std::str::from_utf8(&fmap_raw.name).unwrap_or("")
        }
        .to_string();

        FMap {
            name: fmap_name,
            version_major: fmap_raw.version_major,
            version_minor: fmap_raw.version_minor,
            base: fmap_raw.base,
            size: fmap_raw.size,
            areas: Vec::new(),
        }
    }
}

impl FMap {
    pub fn parse_fmap(reader: &mut (impl Read + Seek)) -> Result<FMap, FMapError> {
        let mut buffer = [0_u8; mem::size_of::<FMapRaw>()];
        if let Err(e) = reader.read_exact(&mut buffer) {
            return Err(FMapError::from(e));
        }

        let fmap_raw: FMapRaw = unsafe { mem::transmute(buffer) };

        if fmap_raw.version_major != VERSION_MAJOR as u8 {
            return Err(FMapError::IncorrectVersion(
                fmap_raw.version_major,
                fmap_raw.version_minor,
            ));
        }

        let fmap_nareas = fmap_raw.nareas;
        let mut fmap = FMap::from(fmap_raw);

        // Read areas
        for _ in 0..fmap_nareas {
            let mut buffer = [0_u8; mem::size_of::<FMapAreaRaw>()];
            if let Err(e) = reader.read_exact(&mut buffer) {
                return Err(FMapError::from(e));
            }

            let fmap_area_raw: FMapAreaRaw = unsafe { mem::transmute(buffer) };
            fmap.areas.push(FMapArea::from(fmap_area_raw));
        }

        Ok(fmap)
    }

    fn is_fmap(reader: &mut impl Read) -> Result<bool, std::io::Error> {
        let mut signature_buffer = [0; SIGNATURE.len()];
        reader.read_exact(&mut signature_buffer)?;
        Ok(signature_buffer == *SIGNATURE)
    }

    /// Returns FMap and offset of that fmap on success.
    pub fn find_fmap(reader: &mut (impl Read + Seek)) -> Result<(FMap, usize), FMapError> {
        let data_size = reader.seek(SeekFrom::End(0))?;

        if HEADER_SIZE as u64 >= data_size {
            return Err(FMapError::from(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Not enough data to fit FMap",
            )));
        }

        // Quick check at the beginning for directly passed FMap.
        reader.seek(SeekFrom::Start(0))?;
        match Self::is_fmap(reader) {
            Ok(true) => {
                reader.seek(SeekFrom::Start(0))?;
                let fmap = Self::parse_fmap(reader)?;
                return Ok((fmap, 0));
            }
            Err(e) => return Err(FMapError::from(e)),
            _ => (),
        }

        let limit = data_size as usize - HEADER_SIZE;

        // Search from largest alignments to find FMap instead of strings.
        let align_log = ((limit - 1) as f64).log2();
        let mut align = 2usize.pow(align_log as u32);

        while align >= SEARCH_STRIDE {
            let mut offset = align;
            while offset <= limit {
                reader.seek(SeekFrom::Start(offset as u64))?;
                match Self::is_fmap(reader) {
                    Ok(true) => {
                        reader.seek(SeekFrom::Start(offset as u64))?;
                        let fmap = Self::parse_fmap(reader)?;
                        return Ok((fmap, offset));
                    }
                    Err(e) => return Err(FMapError::from(e)),
                    _ => (),
                }

                offset += align;
            }
            align /= 2;
        }

        Err(FMapError::NotFound)
    }

    pub fn get(&self, area_name: &str) -> Option<&FMapArea> {
        self.areas.iter().find(|&ar| ar.name == *area_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use std::fs::File;
    use std::io::Cursor;

    const EXAMPLE_FMAP_BIN_DATA_OFFSET: usize = 0x200;

    #[test]
    fn test_is_fmap() -> Result<(), String> {
        let mut reader_ok = Cursor::new(&SIGNATURE[..]);
        match FMap::is_fmap(&mut reader_ok) {
            Ok(false) => {
                return Err(
                    "FMap::is_fmap() expected to return true for correct signature".to_string(),
                )
            }
            Err(e) => return Err(e.to_string()),
            Ok(true) => (),
        }

        let incorrect_signature = "__NOT_FMAP__".as_bytes();
        let mut reader_not_ok = Cursor::new(incorrect_signature);
        match FMap::is_fmap(&mut reader_not_ok) {
            Ok(true) => {
                Err("FMap::is_fmap() expected to return false for incorrect signature".to_string())
            }
            Err(e) => Err(e.to_string()),
            Ok(false) => Ok(()),
        }
    }

    #[test]
    fn test_find_fmap_with_example_file() -> Result<(), String> {
        let mut d = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        d.push("resources/test/example_fmap.bin");

        let mut fmap_file = match File::open(&d) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "Failed to open test resource file `{}'. Error: {e}",
                    d
                ))
            }
        };

        fn check_example_fmap(
            reader: &mut (impl Read + Seek),
            hit_at: usize,
        ) -> Result<(), String> {
            let (fmap, hit_offset) = match FMap::find_fmap(reader) {
                Ok(v) => v,
                Err(e) => return Err(format!("Faild to parse expected correct FMap. Error: {e}")),
            };
            // # name                     start       end         size
            // -entire flash-             00000000    00000400    00000400
            //   bootblock                  00000000    00000080    00000080
            //   normal                     00000080    00000100    00000080
            //   fallback                   00000100    00000200    00000100
            //   data                       00000200    00000400    00000200

            assert_eq!(hit_offset, hit_at);

            assert_eq!(fmap.base, 0);
            assert_eq!(fmap.size, 0x400);
            assert_eq!(fmap.version_major, 1);
            assert_eq!(fmap.version_minor, 0);
            assert_eq!(fmap.name, "example");
            assert_eq!(fmap.areas.len(), 4);

            let a1 = &fmap.areas[0];
            assert_eq!(a1.name, "bootblock");
            assert_eq!(a1.offset, 0);
            assert_eq!(a1.size, 0x80);
            assert_eq!(a1.flags, FMapFlags::Static);

            let a2 = &fmap.areas[1];
            assert_eq!(a2.name, "normal");
            assert_eq!(a2.offset, 0x80);
            assert_eq!(a2.size, 0x80);
            assert_eq!(a2.flags, FMapFlags::Static | FMapFlags::Compressed);

            let a3 = &fmap.areas[2];
            assert_eq!(a3.name, "fallback");
            assert_eq!(a3.offset, 0x100);
            assert_eq!(a3.size, 0x100);
            assert_eq!(a3.flags, FMapFlags::Static | FMapFlags::Compressed);

            let a4 = &fmap.areas[3];
            assert_eq!(a4.name, "data");
            assert_eq!(a4.offset, EXAMPLE_FMAP_BIN_DATA_OFFSET as u32);
            assert_eq!(a4.size, EXAMPLE_FMAP_BIN_DATA_OFFSET as u32);
            assert_eq!(a4.flags, FMapFlags::empty());

            Ok(())
        }

        check_example_fmap(&mut fmap_file, EXAMPLE_FMAP_BIN_DATA_OFFSET)?;

        let fmap_data = match std::fs::read(&d) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "Failed to open test resource file `{}'. Error: {e}",
                    d
                ))
            }
        };
        let mut c = Cursor::new(&fmap_data[EXAMPLE_FMAP_BIN_DATA_OFFSET..]);

        check_example_fmap(&mut c, 0)
    }

    #[test]
    fn test_find_fmap_not_enough_data() -> Result<(), String> {
        let data = "not_enough_data_for_fmap".as_bytes().to_vec();
        let mut reader = Cursor::new(data);
        match FMap::find_fmap(&mut reader) {
            Ok(_) => Err("FMap::find_fmap expected to fail but succeded".into()),
            Err(FMapError::IOError { .. }) => Ok(()),
            Err(e) => Err(format!("Unexpected error: {e}")),
        }
    }

    #[test]
    fn test_find_fmap_incorrect_alignment() -> Result<(), String> {
        let mut d = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        d.push("resources/test/example_fmap.bin");

        let fmap_data = match std::fs::read(&d) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "Failed to open test resource file `{}'. Error: {e}",
                    d
                ))
            }
        };
        let mut c = Cursor::new(&fmap_data[3..]);

        match FMap::find_fmap(&mut c) {
            Ok(_) => Err("FMap::find_fmap expected to fail but succeded".into()),
            Err(FMapError::NotFound) => Ok(()),
            Err(e) => Err(format!("Unexpected error: {e}")),
        }
    }

    #[test]
    fn test_find_fmap_incorrect_version() -> Result<(), String> {
        let mut d = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        d.push("resources/test/example_fmap.bin");

        let mut fmap_data = match std::fs::read(&d) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "Failed to open test resource file `{}'. Error: {e}",
                    d
                ))
            }
        };

        fmap_data[EXAMPLE_FMAP_BIN_DATA_OFFSET + SIGNATURE.len()] = 6;
        fmap_data[EXAMPLE_FMAP_BIN_DATA_OFFSET + SIGNATURE.len() + 1] = 6;

        match FMap::find_fmap(&mut Cursor::new(fmap_data)) {
            Ok(_) => Err("FMap::find_fmap expected to fail but succeded".into()),
            Err(FMapError::IncorrectVersion(_, _)) => Ok(()),
            Err(e) => Err(format!("Unexpected error: {e}")),
        }
    }

    #[test]
    fn test_fmap_get() -> Result<(), String> {
        let fmap = FMap {
            name: "example".to_string(),
            version_major: 1,
            version_minor: 0,
            base: 0,
            size: 1024,
            areas: vec![FMapArea {
                name: "bootblock".to_string(),
                offset: 0,
                size: 128,
                flags: FMapFlags::empty(),
            }],
        };

        match fmap.get("bootblock") {
            None => return Err("FMapArea expected, got None".to_string()),
            Some(a) => assert_eq!(a, &fmap.areas[0]),
        }

        if let Some(a) = fmap.get("not_found") {
            return Err(format!("Expected None, got FMapArea: {a:?}"));
        }

        Ok(())
    }
}
