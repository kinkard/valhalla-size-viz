use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    Identity,
    Gzip,
    Zstd,
}

impl Encoding {
    pub fn as_header_value(self) -> &'static str {
        match self {
            Encoding::Identity => "identity",
            Encoding::Gzip => "gzip",
            Encoding::Zstd => "zstd",
        }
    }
}

// Valhalla graph tile grids: level 0 = highway (4° tiles, 90×45 cols×rows),
// level 1 = arterial (1°, 360×180), level 2 = local (0.25°, 1440×720).
// Tile IDs are row-major (id = row * cols + col), so max_tile_id = cols * rows - 1.
// Kept as a static table because these are protocol constants baked into Valhalla,
// not configuration.
pub const MAX_TILE_IDS: [u32; 3] = [90 * 45 - 1, 360 * 180 - 1, 1440 * 720 - 1];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileId {
    pub level: u8,
    pub id: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TileIdError {
    #[error("invalid level {level}, expected 0, 1, or 2")]
    InvalidLevel { level: u8 },
    #[error("tile id {id} out of range for level {level} (max {max})")]
    IdOutOfRange { level: u8, id: u32, max: u32 },
}

impl TileId {
    pub fn to_path(self) -> String {
        let dir = self.id / 1000;
        let file = self.id % 1000;
        format!("{}/{:03}/{:03}.gph", self.level, dir, file)
    }

    pub fn validate(self) -> Result<(), TileIdError> {
        let max = MAX_TILE_IDS
            .get(self.level as usize)
            .ok_or(TileIdError::InvalidLevel { level: self.level })?;
        if self.id > *max {
            return Err(TileIdError::IdOutOfRange {
                level: self.level,
                id: self.id,
                max: *max,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn encoding_header_values() {
        assert_eq!(Encoding::Identity.as_header_value(), "identity");
        assert_eq!(Encoding::Gzip.as_header_value(), "gzip");
        assert_eq!(Encoding::Zstd.as_header_value(), "zstd");
    }

    #[test]
    fn encoding_deserialize_accepts_known_lowercase() {
        assert_eq!(
            serde_json::from_value::<Encoding>(json!("identity")).unwrap(),
            Encoding::Identity
        );
        assert_eq!(
            serde_json::from_value::<Encoding>(json!("gzip")).unwrap(),
            Encoding::Gzip
        );
        assert_eq!(
            serde_json::from_value::<Encoding>(json!("zstd")).unwrap(),
            Encoding::Zstd
        );
    }

    #[test]
    fn encoding_deserialize_rejects_unknown_and_uppercase() {
        assert!(serde_json::from_value::<Encoding>(json!("br")).is_err());
        assert!(serde_json::from_value::<Encoding>(json!("Gzip")).is_err());
        assert!(serde_json::from_value::<Encoding>(json!("ZSTD")).is_err());
        assert!(serde_json::from_value::<Encoding>(json!("")).is_err());
    }

    #[test]
    fn to_path_matches_js_reference() {
        // mirrors `tileIdToPath` in ../sar-tiles-viz/web/index.html
        assert_eq!(
            TileId {
                level: 2,
                id: 818660
            }
            .to_path(),
            "2/818/660.gph"
        );
        assert_eq!(TileId { level: 0, id: 529 }.to_path(), "0/000/529.gph");
        assert_eq!(TileId { level: 1, id: 0 }.to_path(), "1/000/000.gph");
        assert_eq!(
            TileId {
                level: 2,
                id: 1_000
            }
            .to_path(),
            "2/001/000.gph"
        );
    }

    #[test]
    fn validate_rejects_bad_level() {
        let err = TileId { level: 3, id: 0 }.validate().unwrap_err();
        assert_eq!(err, TileIdError::InvalidLevel { level: 3 });
        let err = TileId { level: 99, id: 0 }.validate().unwrap_err();
        assert_eq!(err, TileIdError::InvalidLevel { level: 99 });
    }

    #[test]
    fn validate_rejects_id_out_of_range() {
        let err = TileId {
            level: 0,
            id: 4_050,
        }
        .validate()
        .unwrap_err();
        assert_eq!(
            err,
            TileIdError::IdOutOfRange {
                level: 0,
                id: 4_050,
                max: 4_049
            }
        );
        let err = TileId {
            level: 2,
            id: 1_036_800,
        }
        .validate()
        .unwrap_err();
        assert_eq!(
            err,
            TileIdError::IdOutOfRange {
                level: 2,
                id: 1_036_800,
                max: 1_036_799
            }
        );
    }

    #[test]
    fn validate_accepts_boundary_values() {
        assert!(TileId { level: 0, id: 0 }.validate().is_ok());
        assert!(
            TileId {
                level: 0,
                id: 4_049
            }
            .validate()
            .is_ok()
        );
        assert!(
            TileId {
                level: 1,
                id: 64_799
            }
            .validate()
            .is_ok()
        );
        assert!(
            TileId {
                level: 2,
                id: 1_036_799
            }
            .validate()
            .is_ok()
        );
    }
}
