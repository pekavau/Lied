//! WebDAV path ↔ entity-tree resolution.
//!
//! Parses a request path under the two WebDAV trees into a [`ResolvedPath`]
//! naming its position in the Lied entity model (CLAUDE.md "WebDAV layout").
//! This is pure structural parsing — it does not touch the database, so slugs
//! here are *candidate* slugs that the filesystem layer still resolves to live
//! rows (and applies soft-delete / per-role visibility to).
//!
//! Three trees:
//! - `/orgs/<org>/arrangements/<arr>/{score,voices/<voice>/{<file>,annotations/<user>/<file>}}`
//! - `/orgs/<org>/collections/<coll>/<index>-<arr>/{score,voices/<voice>/<file>}` —
//!   a read-only computed view assembled from `CollectionItem`s +
//!   `PartAssignment`s (issue #10). No annotations subtree here: annotations
//!   are addressed via the arrangements tree only.
//! - `/users/<user>/library/<rel…>` (private, files-by-convention, no `File` rows)

/// The structural keyword segments that are part of the fixed tree shape rather
/// than user-named slugs.
const ARRANGEMENTS: &str = "arrangements";
const COLLECTIONS: &str = "collections";
const SCORE: &str = "score";
const VOICES: &str = "voices";
const ANNOTATIONS: &str = "annotations";
const LIBRARY: &str = "library";

/// Split a collections item directory segment (`<index>-<arrangement-slug>`,
/// e.g. `1-bolero`) into `(index, arrangement_slug)`. Splits on the *first* `-`
/// only, since the index is always a plain non-negative integer with no `-` of
/// its own (this exactly inverts the `<index>-<slug>` construction the
/// filesystem layer uses when listing a collection) — the arrangement slug may
/// itself contain further hyphens. Returns `None` if the segment doesn't have
/// the `<digits>-<rest>` shape, which the caller maps to `404 Not Found`.
fn split_item_segment(seg: &str) -> Option<(i32, &str)> {
    let dash = seg.find('-')?;
    let (idx_str, rest) = seg.split_at(dash);
    let index: i32 = idx_str.parse().ok()?;
    let arr_slug = &rest[1..];
    if arr_slug.is_empty() {
        return None;
    }
    Some((index, arr_slug))
}

/// A WebDAV path resolved to its position in the entity tree. Every variant is
/// either a directory node or a leaf file node; [`ResolvedPath::is_collection`]
/// reports which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPath {
    /// `/` — synthetic root listing `orgs` and `users`.
    Root,

    // ── org tree ────────────────────────────────────────────────────────────
    /// `/orgs`
    OrgsRoot,
    /// `/orgs/<org>`
    Org { org: String },
    /// `/orgs/<org>/arrangements`
    ArrangementsRoot { org: String },
    /// `/orgs/<org>/arrangements/<arr>`
    Arrangement { org: String, arr: String },
    /// `/orgs/<org>/arrangements/<arr>/score`
    ScoreDir { org: String, arr: String },
    /// `/orgs/<org>/arrangements/<arr>/score/<file>`
    ScoreFile {
        org: String,
        arr: String,
        file: String,
    },
    /// `/orgs/<org>/arrangements/<arr>/voices`
    VoicesDir { org: String, arr: String },
    /// `/orgs/<org>/arrangements/<arr>/voices/<voice>`
    Voice {
        org: String,
        arr: String,
        voice: String,
    },
    /// `/orgs/<org>/arrangements/<arr>/voices/<voice>/<file>`
    VoiceFile {
        org: String,
        arr: String,
        voice: String,
        file: String,
    },
    /// `/orgs/<org>/arrangements/<arr>/voices/<voice>/annotations`
    AnnotationsDir {
        org: String,
        arr: String,
        voice: String,
    },
    /// `/orgs/<org>/arrangements/<arr>/voices/<voice>/annotations/<user>`
    AnnotationsUserDir {
        org: String,
        arr: String,
        voice: String,
        user: String,
    },
    /// `/orgs/<org>/arrangements/<arr>/voices/<voice>/annotations/<user>/<file>`
    AnnotationFile {
        org: String,
        arr: String,
        voice: String,
        user: String,
        file: String,
    },

    // ── collections tree (read-only computed view, issue #10) ───────────────
    /// `/orgs/<org>/collections`
    CollectionsRoot { org: String },
    /// `/orgs/<org>/collections/<coll>`
    Collection { org: String, coll: String },
    /// `/orgs/<org>/collections/<coll>/<index>-<arr>` — the `<index>-<arr-slug>`
    /// segment is split here into its numeric `index` and candidate
    /// `arr_slug`; the filesystem layer resolves both against the live
    /// `CollectionItem` (the index locates the item, the slug is verified
    /// against it).
    CollectionItemDir {
        org: String,
        coll: String,
        index: i32,
        arr_slug: String,
    },
    /// `/orgs/<org>/collections/<coll>/<index>-<arr>/score`
    CollectionScoreDir {
        org: String,
        coll: String,
        index: i32,
        arr_slug: String,
    },
    /// `/orgs/<org>/collections/<coll>/<index>-<arr>/score/<file>`
    CollectionScoreFile {
        org: String,
        coll: String,
        index: i32,
        arr_slug: String,
        file: String,
    },
    /// `/orgs/<org>/collections/<coll>/<index>-<arr>/voices`
    CollectionVoicesDir {
        org: String,
        coll: String,
        index: i32,
        arr_slug: String,
    },
    /// `/orgs/<org>/collections/<coll>/<index>-<arr>/voices/<voice>`
    CollectionVoice {
        org: String,
        coll: String,
        index: i32,
        arr_slug: String,
        voice: String,
    },
    /// `/orgs/<org>/collections/<coll>/<index>-<arr>/voices/<voice>/<file>`
    CollectionVoiceFile {
        org: String,
        coll: String,
        index: i32,
        arr_slug: String,
        voice: String,
        file: String,
    },

    // ── user library tree ────────────────────────────────────────────────────
    /// `/users`
    UsersRoot,
    /// `/users/<user>`
    UserHome { user: String },
    /// `/users/<user>/library`
    LibraryRoot { user: String },
    /// `/users/<user>/library/<rel…>` — an arbitrary file or directory inside a
    /// user's private library. Whether it is a file or directory is not knowable
    /// from the path alone (the library is free-form storage, not a structured
    /// tree), so the filesystem layer resolves that against MinIO.
    LibraryEntry { user: String, rel: Vec<String> },
}

impl ResolvedPath {
    /// Parse a request path (e.g. `/orgs/acme/arrangements/bolero/score`) into a
    /// [`ResolvedPath`]. Returns `None` for a path that does not fit either
    /// tree's shape (which the filesystem maps to `404 Not Found`).
    ///
    /// Segments are expected already URL-decoded (dav-server's `DavPath` decodes
    /// before we see them). Empty segments (leading/trailing/double slashes) are
    /// ignored.
    pub fn parse(path: &str) -> Option<ResolvedPath> {
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        Self::from_segments(&segs)
    }

    fn from_segments(segs: &[&str]) -> Option<ResolvedPath> {
        use ResolvedPath::*;

        // Reject any segment that could escape the tree or is otherwise unsafe
        // as a slug / filename. `DavPath` already forbids `..`, but be explicit.
        if segs.iter().any(|s| *s == "." || *s == ".." || s.is_empty()) {
            return None;
        }

        match segs {
            [] => Some(Root),

            ["orgs"] => Some(OrgsRoot),
            ["orgs", org] => Some(Org {
                org: (*org).to_string(),
            }),
            ["orgs", org, ARRANGEMENTS] => Some(ArrangementsRoot {
                org: (*org).to_string(),
            }),
            ["orgs", org, ARRANGEMENTS, arr] => Some(Arrangement {
                org: (*org).to_string(),
                arr: (*arr).to_string(),
            }),

            // score/
            ["orgs", org, ARRANGEMENTS, arr, SCORE] => Some(ScoreDir {
                org: (*org).to_string(),
                arr: (*arr).to_string(),
            }),
            ["orgs", org, ARRANGEMENTS, arr, SCORE, file] => Some(ScoreFile {
                org: (*org).to_string(),
                arr: (*arr).to_string(),
                file: (*file).to_string(),
            }),

            // voices/
            ["orgs", org, ARRANGEMENTS, arr, VOICES] => Some(VoicesDir {
                org: (*org).to_string(),
                arr: (*arr).to_string(),
            }),
            ["orgs", org, ARRANGEMENTS, arr, VOICES, voice] => Some(Voice {
                org: (*org).to_string(),
                arr: (*arr).to_string(),
                voice: (*voice).to_string(),
            }),

            // voices/<voice>/annotations/… — must be matched before the plain
            // voice-file arm, since "annotations" is a reserved child name.
            ["orgs", org, ARRANGEMENTS, arr, VOICES, voice, ANNOTATIONS] => Some(AnnotationsDir {
                org: (*org).to_string(),
                arr: (*arr).to_string(),
                voice: (*voice).to_string(),
            }),
            ["orgs", org, ARRANGEMENTS, arr, VOICES, voice, ANNOTATIONS, user] => {
                Some(AnnotationsUserDir {
                    org: (*org).to_string(),
                    arr: (*arr).to_string(),
                    voice: (*voice).to_string(),
                    user: (*user).to_string(),
                })
            }
            ["orgs", org, ARRANGEMENTS, arr, VOICES, voice, ANNOTATIONS, user, file] => {
                Some(AnnotationFile {
                    org: (*org).to_string(),
                    arr: (*arr).to_string(),
                    voice: (*voice).to_string(),
                    user: (*user).to_string(),
                    file: (*file).to_string(),
                })
            }

            // voices/<voice>/<file> — any non-reserved child of a voice.
            ["orgs", org, ARRANGEMENTS, arr, VOICES, voice, file] => Some(VoiceFile {
                org: (*org).to_string(),
                arr: (*arr).to_string(),
                voice: (*voice).to_string(),
                file: (*file).to_string(),
            }),

            // collections/ (read-only computed view) — must be matched at the
            // same level as ARRANGEMENTS, before the catch-all.
            ["orgs", org, COLLECTIONS] => Some(CollectionsRoot {
                org: (*org).to_string(),
            }),
            ["orgs", org, COLLECTIONS, coll] => Some(Collection {
                org: (*org).to_string(),
                coll: (*coll).to_string(),
            }),
            ["orgs", org, COLLECTIONS, coll, item] => {
                let (index, arr_slug) = split_item_segment(item)?;
                Some(CollectionItemDir {
                    org: (*org).to_string(),
                    coll: (*coll).to_string(),
                    index,
                    arr_slug: arr_slug.to_string(),
                })
            }
            ["orgs", org, COLLECTIONS, coll, item, SCORE] => {
                let (index, arr_slug) = split_item_segment(item)?;
                Some(CollectionScoreDir {
                    org: (*org).to_string(),
                    coll: (*coll).to_string(),
                    index,
                    arr_slug: arr_slug.to_string(),
                })
            }
            ["orgs", org, COLLECTIONS, coll, item, SCORE, file] => {
                let (index, arr_slug) = split_item_segment(item)?;
                Some(CollectionScoreFile {
                    org: (*org).to_string(),
                    coll: (*coll).to_string(),
                    index,
                    arr_slug: arr_slug.to_string(),
                    file: (*file).to_string(),
                })
            }
            ["orgs", org, COLLECTIONS, coll, item, VOICES] => {
                let (index, arr_slug) = split_item_segment(item)?;
                Some(CollectionVoicesDir {
                    org: (*org).to_string(),
                    coll: (*coll).to_string(),
                    index,
                    arr_slug: arr_slug.to_string(),
                })
            }
            ["orgs", org, COLLECTIONS, coll, item, VOICES, voice] => {
                let (index, arr_slug) = split_item_segment(item)?;
                Some(CollectionVoice {
                    org: (*org).to_string(),
                    coll: (*coll).to_string(),
                    index,
                    arr_slug: arr_slug.to_string(),
                    voice: (*voice).to_string(),
                })
            }
            ["orgs", org, COLLECTIONS, coll, item, VOICES, voice, file] => {
                let (index, arr_slug) = split_item_segment(item)?;
                Some(CollectionVoiceFile {
                    org: (*org).to_string(),
                    coll: (*coll).to_string(),
                    index,
                    arr_slug: arr_slug.to_string(),
                    voice: (*voice).to_string(),
                    file: (*file).to_string(),
                })
            }

            ["users"] => Some(UsersRoot),
            ["users", user] => Some(UserHome {
                user: (*user).to_string(),
            }),
            ["users", user, LIBRARY] => Some(LibraryRoot {
                user: (*user).to_string(),
            }),
            ["users", user, LIBRARY, rest @ ..] if !rest.is_empty() => Some(LibraryEntry {
                user: (*user).to_string(),
                rel: rest.iter().map(|s| (*s).to_string()).collect(),
            }),

            _ => None,
        }
    }

    /// Whether this node is a directory (collection) rather than a file.
    /// `LibraryEntry` is unknown from the path alone and reported as `false`
    /// here — the filesystem layer decides via MinIO (a library path with no
    /// object but with children is a directory).
    pub fn is_collection(&self) -> bool {
        use ResolvedPath::*;
        matches!(
            self,
            Root | OrgsRoot
                | Org { .. }
                | ArrangementsRoot { .. }
                | Arrangement { .. }
                | ScoreDir { .. }
                | VoicesDir { .. }
                | Voice { .. }
                | AnnotationsDir { .. }
                | AnnotationsUserDir { .. }
                | CollectionsRoot { .. }
                | Collection { .. }
                | CollectionItemDir { .. }
                | CollectionScoreDir { .. }
                | CollectionVoicesDir { .. }
                | CollectionVoice { .. }
                | UsersRoot
                | UserHome { .. }
                | LibraryRoot { .. }
        )
    }

    /// Whether this node lives in the private user-library tree.
    pub fn is_library(&self) -> bool {
        use ResolvedPath::*;
        matches!(
            self,
            UsersRoot | UserHome { .. } | LibraryRoot { .. } | LibraryEntry { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::ResolvedPath::*;
    use super::*;

    fn p(s: &str) -> Option<ResolvedPath> {
        ResolvedPath::parse(s)
    }

    #[test]
    fn roots_and_org_tree() {
        assert_eq!(p("/"), Some(Root));
        assert_eq!(p(""), Some(Root));
        assert_eq!(p("/orgs"), Some(OrgsRoot));
        assert_eq!(p("/orgs/"), Some(OrgsRoot));
        assert_eq!(p("/orgs/acme"), Some(Org { org: "acme".into() }));
        assert_eq!(
            p("/orgs/acme/arrangements"),
            Some(ArrangementsRoot { org: "acme".into() })
        );
        assert_eq!(
            p("/orgs/acme/arrangements/bolero"),
            Some(Arrangement {
                org: "acme".into(),
                arr: "bolero".into()
            })
        );
    }

    #[test]
    fn score_and_voice_files() {
        assert_eq!(
            p("/orgs/acme/arrangements/bolero/score"),
            Some(ScoreDir {
                org: "acme".into(),
                arr: "bolero".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/arrangements/bolero/score/full-score.pdf"),
            Some(ScoreFile {
                org: "acme".into(),
                arr: "bolero".into(),
                file: "full-score.pdf".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/arrangements/bolero/voices"),
            Some(VoicesDir {
                org: "acme".into(),
                arr: "bolero".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/arrangements/bolero/voices/flute-1"),
            Some(Voice {
                org: "acme".into(),
                arr: "bolero".into(),
                voice: "flute-1".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/arrangements/bolero/voices/flute-1/part.pdf"),
            Some(VoiceFile {
                org: "acme".into(),
                arr: "bolero".into(),
                voice: "flute-1".into(),
                file: "part.pdf".into()
            })
        );
    }

    #[test]
    fn annotations_reserved_name_wins_over_voice_file() {
        // "annotations" under a voice is the annotations dir, not a voice file.
        assert_eq!(
            p("/orgs/acme/arrangements/bolero/voices/flute-1/annotations"),
            Some(AnnotationsDir {
                org: "acme".into(),
                arr: "bolero".into(),
                voice: "flute-1".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/arrangements/bolero/voices/flute-1/annotations/alice"),
            Some(AnnotationsUserDir {
                org: "acme".into(),
                arr: "bolero".into(),
                voice: "flute-1".into(),
                user: "alice".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/arrangements/bolero/voices/flute-1/annotations/alice/bowings.pdf"),
            Some(AnnotationFile {
                org: "acme".into(),
                arr: "bolero".into(),
                voice: "flute-1".into(),
                user: "alice".into(),
                file: "bowings.pdf".into()
            })
        );
    }

    #[test]
    fn collections_tree() {
        assert_eq!(
            p("/orgs/acme/collections"),
            Some(CollectionsRoot { org: "acme".into() })
        );
        assert_eq!(
            p("/orgs/acme/collections/spring-2026"),
            Some(Collection {
                org: "acme".into(),
                coll: "spring-2026".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/collections/spring-2026/1-bolero"),
            Some(CollectionItemDir {
                org: "acme".into(),
                coll: "spring-2026".into(),
                index: 1,
                arr_slug: "bolero".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/collections/spring-2026/1-bolero/score"),
            Some(CollectionScoreDir {
                org: "acme".into(),
                coll: "spring-2026".into(),
                index: 1,
                arr_slug: "bolero".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/collections/spring-2026/1-bolero/score/full.pdf"),
            Some(CollectionScoreFile {
                org: "acme".into(),
                coll: "spring-2026".into(),
                index: 1,
                arr_slug: "bolero".into(),
                file: "full.pdf".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/collections/spring-2026/1-bolero/voices"),
            Some(CollectionVoicesDir {
                org: "acme".into(),
                coll: "spring-2026".into(),
                index: 1,
                arr_slug: "bolero".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/collections/spring-2026/1-bolero/voices/flute-1"),
            Some(CollectionVoice {
                org: "acme".into(),
                coll: "spring-2026".into(),
                index: 1,
                arr_slug: "bolero".into(),
                voice: "flute-1".into()
            })
        );
        assert_eq!(
            p("/orgs/acme/collections/spring-2026/1-bolero/voices/flute-1/part.pdf"),
            Some(CollectionVoiceFile {
                org: "acme".into(),
                coll: "spring-2026".into(),
                index: 1,
                arr_slug: "bolero".into(),
                voice: "flute-1".into(),
                file: "part.pdf".into()
            })
        );
    }

    #[test]
    fn collection_item_segment_must_be_index_dash_slug() {
        // A non-numeric index, or a bare slug with no index, is not a valid
        // collection item directory — the parser rejects it (→ 404).
        assert_eq!(p("/orgs/acme/collections/spring-2026/bolero"), None);
        assert_eq!(p("/orgs/acme/collections/spring-2026/1-"), None);
        assert_eq!(p("/orgs/acme/collections/spring-2026/-bolero"), None);
        // The slug may contain further hyphens; only the first split matters.
        assert_eq!(
            p("/orgs/acme/collections/spring-2026/12-la-mer"),
            Some(CollectionItemDir {
                org: "acme".into(),
                coll: "spring-2026".into(),
                index: 12,
                arr_slug: "la-mer".into()
            })
        );
    }

    #[test]
    fn is_collection_classification_for_collections_tree() {
        assert!(p("/orgs/acme/collections").unwrap().is_collection());
        assert!(p("/orgs/acme/collections/spring-2026")
            .unwrap()
            .is_collection());
        assert!(p("/orgs/acme/collections/spring-2026/1-bolero")
            .unwrap()
            .is_collection());
        assert!(p("/orgs/acme/collections/spring-2026/1-bolero/score")
            .unwrap()
            .is_collection());
        assert!(
            !p("/orgs/acme/collections/spring-2026/1-bolero/score/full.pdf")
                .unwrap()
                .is_collection()
        );
        assert!(p("/orgs/acme/collections/spring-2026/1-bolero/voices")
            .unwrap()
            .is_collection());
        assert!(
            p("/orgs/acme/collections/spring-2026/1-bolero/voices/flute-1")
                .unwrap()
                .is_collection()
        );
        assert!(
            !p("/orgs/acme/collections/spring-2026/1-bolero/voices/flute-1/part.pdf")
                .unwrap()
                .is_collection()
        );
    }

    #[test]
    fn user_library_tree() {
        assert_eq!(p("/users"), Some(UsersRoot));
        assert_eq!(
            p("/users/alice"),
            Some(UserHome {
                user: "alice".into()
            })
        );
        assert_eq!(
            p("/users/alice/library"),
            Some(LibraryRoot {
                user: "alice".into()
            })
        );
        assert_eq!(
            p("/users/alice/library/practice/etude.pdf"),
            Some(LibraryEntry {
                user: "alice".into(),
                rel: vec!["practice".into(), "etude.pdf".into()]
            })
        );
    }

    #[test]
    fn rejects_traversal_and_unknown_shapes() {
        assert_eq!(p("/orgs/acme/../secrets"), None);
        assert_eq!(p("/orgs/acme/arrangements/bolero/score/a/b/c"), None);
        assert_eq!(p("/nope"), None);
        assert_eq!(p("/users/alice/notlibrary"), None);
        // A deep path with an unknown reserved word under the arrangement.
        assert_eq!(p("/orgs/acme/arrangements/bolero/bogus"), None);
    }

    #[test]
    fn is_collection_classification() {
        assert!(p("/orgs/acme/arrangements/bolero").unwrap().is_collection());
        assert!(p("/orgs/acme/arrangements/bolero/voices/flute-1")
            .unwrap()
            .is_collection());
        assert!(!p("/orgs/acme/arrangements/bolero/score/x.pdf")
            .unwrap()
            .is_collection());
        assert!(!p("/orgs/acme/arrangements/bolero/voices/flute-1/part.pdf")
            .unwrap()
            .is_collection());
    }
}
