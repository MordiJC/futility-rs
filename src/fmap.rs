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
        if Self::is_fmap(reader)? {
            reader.seek(SeekFrom::Start(0))?;
            let fmap = Self::parse_fmap(reader)?;
            return Ok((fmap, 0));
        }

        let limit = data_size as usize - HEADER_SIZE;

        // Search from largest alignments to find FMap instead of strings.
        let align_log = ((limit - 1) as f64).log2();
        let mut align = 2usize.pow(align_log as u32);

        while align >= SEARCH_STRIDE {
            let mut offset = align;
            while offset <= limit {
                reader.seek(SeekFrom::Start(offset as u64))?;
                if Self::is_fmap(reader)? {
                    reader.seek(SeekFrom::Start(offset as u64))?;
                    let fmap = Self::parse_fmap(reader)?;
                    return Ok((fmap, offset));
                }

                offset += align;
            }
            align /= 2;
        }

        Err(FMapError::NotFound)
    }

    pub fn get(&self, area_name: &String) -> Option<&FMapArea> {
        self.areas.iter().find(|&ar| ar.name == *area_name)
    }
}
