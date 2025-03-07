#![feature(assert_matches)]
#![deny(clippy::all)]

use std::{
    borrow::Cow,
    collections::HashSet,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use itertools::Itertools;
use path_slash::PathExt;
use regex::Regex;
use tracing::{info_span, Span};
use turbopath::{AbsoluteSystemPath, AbsoluteSystemPathBuf, PathError};
use wax::{BuildError, Glob, Pattern};

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum WalkType {
    Files,
    Folders,
    All,
}

pub use walkdir::Error as WalkDirError;

#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    // note: wax 0.5 has a lifetime in the BuildError, so we can't use it here
    #[error("bad pattern {0}: {1}")]
    BadPattern(String, Box<BuildError>),
    #[error("invalid path")]
    InvalidPath,
    #[error("walk error: {0}")]
    WalkError(#[from] walkdir::Error),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    WaxWalk(#[from] wax::WalkError),
    #[error("Internal error on glob {glob}: {error}")]
    InternalError { glob: String, error: String },
    #[error("IO Error: {0}")]
    IO(#[from] std::io::Error),
}

fn join_unix_like_paths(a: &str, b: &str) -> String {
    [a.trim_end_matches('/'), "/", b.trim_start_matches('/')].concat()
}

#[tracing::instrument]
fn preprocess_paths_and_globs(
    base_path: &AbsoluteSystemPath,
    include: &[String],
    exclude: &[String],
) -> Result<(PathBuf, Vec<String>, Vec<String>), WalkError> {
    let base_path_slash = base_path
        .as_std_path()
        .to_slash()
        // Windows drive paths need to be escaped, and ':' is a valid token in unix paths
        .map(|s| s.replace(':', "\\:"))
        .ok_or(WalkError::InvalidPath)?;

    let (include_paths, lowest_segment) = include
        .iter()
        .map(|s| fix_glob_pattern(s))
        .map(|s| join_unix_like_paths(&base_path_slash, &s))
        .filter_map(|s| collapse_path(&s).map(|(s, v)| (s.to_string(), v)))
        .fold(
            (vec![], usize::MAX),
            |(mut vec, lowest_segment), (path, lowest_segment_next)| {
                let lowest_segment = std::cmp::min(lowest_segment, lowest_segment_next);
                vec.push(path); // we stringify here due to lifetime issues
                (vec, lowest_segment)
            },
        );

    let base_path = base_path
        .components()
        .take(
            // this can be usize::MAX if there are no include paths
            lowest_segment.saturating_add(1),
        )
        .collect::<PathBuf>();

    let mut exclude_paths = vec![];
    for split in exclude
        .iter()
        .map(|s| fix_glob_pattern(s))
        .map(|s| join_unix_like_paths(&base_path_slash, &s))
        .filter_map(|g| collapse_path(&g).map(|(s, _)| s.to_string()))
    {
        let split = split.to_string();
        // if the glob ends with a slash, then we need to add a double star,
        // unless it already ends with a double star
        if split.ends_with('/') {
            if split.ends_with("**/") {
                exclude_paths.push(split[..split.len() - 1].to_string());
            } else {
                exclude_paths.push(format!("{}**", split));
            }
        } else if split.ends_with("/**") {
            exclude_paths.push(split);
        } else {
            // Match Go globby behavior. If the glob doesn't already end in /**, add it
            // TODO: The Go version uses system separator. Are we forcing all globs to unix
            // paths?
            exclude_paths.push(format!("{}/**", split));
            exclude_paths.push(split);
        }
    }

    Ok((base_path, include_paths, exclude_paths))
}

fn double_doublestar() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\*\*(?:/\*\*)+").unwrap())
}

fn leading_doublestar() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\*\*(?P<suffix>[^*/]+)").unwrap())
}

fn trailing_doublestar() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?P<prefix>[^*/]+)\*\*").unwrap())
}

pub fn fix_glob_pattern(pattern: &str) -> String {
    // This is a no-op on unix systems, but converts to slashes on windows
    #[cfg(not(windows))]
    let needs_trailing_slash = false;
    #[cfg(windows)]
    let needs_trailing_slash = pattern.ends_with('/') || pattern.ends_with('\\');
    let converted = Path::new(pattern)
        .to_slash()
        .expect("failed to roundtrip through Path");
    // TODO: consider inlining path-slash to handle this bug
    // technically this won't happen on unix, the to_slash conversion
    // is a no-op, so it doesn't strip trailing slashes. path-slash
    // strips trailing _unix_ slashes from windows paths, rather than
    // "converting" (leaving) them.
    let p0 = if needs_trailing_slash {
        format!("{}/", converted)
    } else {
        converted.to_string()
    };
    let p1 = double_doublestar().replace(&p0, "**");
    let p2 = leading_doublestar().replace(&p1, "**/*$suffix");
    let p3 = trailing_doublestar().replace(&p2, "$prefix*/**");

    p3.to_string()
}

/// collapse a path, returning a new path with all the dots and dotdots removed
///
/// also returns the position in the path of the first encountered collapse,
/// for the purposes of calculating the new base path
fn collapse_path(path: &str) -> Option<(Cow<str>, usize)> {
    let mut stack: Vec<&str> = vec![];
    let mut changed = false;
    let is_root = path.starts_with('/');

    // the index of the lowest segment that was collapsed
    // this is defined as the lowest stack size after a collapse
    let mut lowest_index = None;

    for segment in path.trim_start_matches('/').split('/') {
        match segment {
            ".." => {
                stack.pop()?;
                // Set this value post-pop so that we capture
                // the remaining prefix, and not the segment we're
                // about to remove. Note that this gets papered over
                // below when we compare against the current stack length.
                lowest_index.get_or_insert(stack.len());
                changed = true;
            }
            "." => {
                lowest_index.get_or_insert(stack.len());
                changed = true;
            }
            _ => stack.push(segment),
        }
        if let Some(lowest_index) = lowest_index.as_mut() {
            *lowest_index = (*lowest_index).min(stack.len());
        }
    }

    let lowest_index = lowest_index.unwrap_or(stack.len());
    if !changed {
        Some((Cow::Borrowed(path), lowest_index))
    } else {
        let string = if is_root {
            std::iter::once("").chain(stack).join("/")
        } else {
            stack.join("/")
        };

        Some((Cow::Owned(string), lowest_index))
    }
}

#[tracing::instrument]
fn glob_with_contextual_error<S: AsRef<str> + std::fmt::Debug>(
    raw: S,
) -> Result<Glob<'static>, WalkError> {
    let raw = raw.as_ref();
    Glob::new(raw)
        .map(|g| g.into_owned())
        .map_err(|e| WalkError::BadPattern(raw.to_string(), Box::new(e)))
}

#[tracing::instrument]
pub fn globwalk(
    base_path: &AbsoluteSystemPath,
    include: &[String],
    exclude: &[String],
    walk_type: WalkType,
) -> Result<HashSet<AbsoluteSystemPathBuf>, WalkError> {
    let (base_path_new, include_paths, exclude_paths) =
        preprocess_paths_and_globs(base_path, include, exclude)?;
    let inc_patterns = include_paths
        .iter()
        .map(glob_with_contextual_error)
        .collect::<Result<Vec<_>, WalkError>>()?;
    let ex_patterns = exclude_paths
        .iter()
        .map(glob_with_contextual_error)
        .collect::<Result<Vec<_>, _>>()?;

    let span = Span::current();
    let result = inc_patterns
        .into_iter()
        .flat_map(|glob| {
            let span =
                tracing::info_span!(parent: &span, &"walk_glob", glob = glob.to_string().as_str());
            let _enter = span.enter();
            // Check if the glob specifies an exact filename with no meta characters.
            if let Some(prefix) = glob.variance().path() {
                // We expect all of our globs to be absolute paths (asserted above)
                assert!(prefix.is_absolute(), "Found relative glob path {}", glob);
                // We're either going to return this path or nothing. Check if it's a directory
                // and if we want directories
                match AbsoluteSystemPathBuf::try_from(prefix).and_then(|path| {
                    let metadata = path.symlink_metadata()?;
                    Ok((path, metadata))
                }) {
                    Err(e) if e.is_io_error(ErrorKind::NotFound) => {
                        // If the file doesn't exist, it's not an error, there's just nothing to
                        // glob
                        vec![]
                    }
                    Err(e) => vec![Err(e.into())],
                    Ok((_, md)) if walk_type == WalkType::Files && md.is_dir() => {
                        vec![]
                    }
                    Ok((path, _)) => vec![Ok(path)],
                }
            } else {
                glob.walk(&base_path_new)
                    .not(ex_patterns.clone())
                    // Per docs, only fails if exclusion list is too large, since we're using
                    // pre-compiled globs
                    .unwrap_or_else(|e| {
                        panic!(
                            "Failed to compile exclusion globs: {:?}: {}",
                            ex_patterns, e,
                        )
                    })
                    .filter_map(|entry| {
                        let span = info_span!(parent: &span, "visit_file", entry = ?entry);
                        let _enter = span.enter();
                        match entry {
                            Ok(entry)
                                if walk_type == WalkType::Files && entry.file_type().is_dir() =>
                            {
                                None
                            }
                            Ok(entry) => Some(
                                AbsoluteSystemPathBuf::try_from(entry.path()).map_err(|e| e.into()),
                            ),
                            Err(e) => {
                                let io_err = std::io::Error::from(e);
                                if io_err.kind() == std::io::ErrorKind::NotFound {
                                    None
                                } else {
                                    Some(Err(io_err.into()))
                                }
                            }
                        }
                    })
                    .collect::<Vec<_>>()
            }
        })
        .collect::<Result<HashSet<_>, WalkError>>()?;
    Ok(result)
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use itertools::Itertools;
    use test_case::test_case;
    use turbopath::AbsoluteSystemPathBuf;

    use crate::{collapse_path, fix_glob_pattern, globwalk, WalkError, WalkType};

    #[cfg(unix)]
    const ROOT: &str = "/";
    #[cfg(windows)]
    const ROOT: &str = "C:\\";
    #[cfg(unix)]
    const GLOB_ROOT: &str = "/";
    #[cfg(windows)]
    const GLOB_ROOT: &str = "C\\:/"; // in globs, expect an escaped ':' token

    #[test_case("a", "a" ; "no change")]
    #[test_case("**/**", "**")]
    #[test_case("**/**/**", "**" ; "Triple doublestar")]
    #[test_case("**token/foo", "**/*token/foo")]
    #[test_case("**token**", "**/*token*/**")]
    fn test_fix_glob_pattern(input: &str, expected: &str) {
        let output = fix_glob_pattern(input);
        assert_eq!(output, expected);
    }

    #[test_case("a/./././b", "a/b", 1 ; "test path with dot segments")]
    #[test_case("a/../b", "b", 0 ; "test path with dotdot segments")]
    #[test_case("a/./../b", "b", 0 ; "test path with mixed dot and dotdot segments")]
    #[test_case("./a/b", "a/b", 0 ; "test path starting with dot segment")]
    #[test_case("a/b/..", "a", 1 ; "test path ending with dotdot segment")]
    #[test_case("a/b/.", "a/b", 2 ; "test path ending with dot segment")]
    #[test_case("a/.././b", "b", 0 ; "test path with mixed and consecutive ./ and ../ segments")]
    #[test_case("/a/./././b", "/a/b", 1 ; "test path with leading / and ./ segments")]
    #[test_case("/a/../b", "/b", 0 ; "test path with leading / and dotdot segments")]
    #[test_case("/a/./../b", "/b", 0 ; "test path with leading / and mixed dot and dotdot segments")]
    #[test_case("/./a/b", "/a/b", 0 ; "test path with leading / and starting with dot segment")]
    #[test_case("/a/b/..", "/a", 1 ; "test path with leading / and ending with dotdot segment")]
    #[test_case("/a/b/.", "/a/b", 2 ; "test path with leading / and ending with dot segment")]
    #[test_case("/a/.././b", "/b", 0 ; "test path with leading / and mixed and consecutive dot and dotdot segments")]
    #[test_case("/a/b/c/../../d/e/f/g/h/i/../j", "/a/d/e/f/g/h/j", 1 ; "leading collapse followed by shorter one")]
    fn test_collapse_path(glob: &str, expected: &str, earliest_collapsed_segement: usize) {
        let (glob, segment) = collapse_path(glob).unwrap();
        assert_eq!(glob, expected);
        assert_eq!(segment, earliest_collapsed_segement);
    }

    #[test_case("../a/b" ; "test path starting with ../ segment should return None")]
    #[test_case("/../a" ; "test path with leading dotdotdot segment should return None")]
    fn test_collapse_path_not(glob: &str) {
        assert_eq!(collapse_path(glob), None);
    }

    #[test_case("a/b/c/d", &["/e/../../../f"], &[], "a/b", None, None ; "can traverse beyond the root")]
    #[test_case("a/b/c/d/", &["/e/../../../f"], &[], "a/b", None, None ; "can handle slash-trailing base path")]
    #[test_case("a/b/c/d/", &["e/../../../f"], &[], "a/b", None, None ; "can handle no slash on glob")]
    #[test_case("a/b/c/d", &["e/../../../f"], &[], "a/b", None, None ; "can handle no slash on either")]
    #[test_case("a/b/c/d", &["/e/f/../g"], &[], "a/b/c/d", None, None ; "can handle no collapse")]
    #[test_case("a/b/c/d", &["./././../.."], &[], "a/b", None, None ; "can handle dot followed by dotdot")]
    #[test_case("a/b/c/d", &["**"], &["**/"], "a/b/c/d", None, Some(&["a/b/c/d/**"]) ; "can handle dot followed by dotdot and dot")]
    #[test_case("a/b/c", &["**"], &["d/"], "a/b/c", None, Some(&["a/b/c/d/**"]) ; "will exclude all subfolders")]
    #[test_case("a/b/c", &["**"], &["d"], "a/b/c", None, Some(&["a/b/c/d/**", "a/b/c/d"]) ; "will exclude all subfolders and file")]
    fn preprocess_paths_and_globs(
        base_path: &str,
        include: &[&str],
        exclude: &[&str],
        base_path_exp: &str,
        include_exp: Option<&[&str]>,
        exclude_exp: Option<&[&str]>,
    ) {
        let raw_path = format!("{}{}", ROOT, base_path);
        let base_path = AbsoluteSystemPathBuf::new(raw_path).unwrap();
        let include = include.iter().map(|s| s.to_string()).collect_vec();
        let exclude = exclude.iter().map(|s| s.to_string()).collect_vec();

        let (result_path, include, exclude) =
            super::preprocess_paths_and_globs(&base_path, &include, &exclude).unwrap();

        let expected = format!(
            "{}{}",
            ROOT,
            base_path_exp.replace('/', std::path::MAIN_SEPARATOR_STR)
        );
        assert_eq!(result_path.to_string_lossy(), expected);

        if let Some(include_exp) = include_exp {
            assert_eq!(
                include,
                include_exp
                    .iter()
                    .map(|s| format!("{}{}", GLOB_ROOT, s))
                    .collect_vec()
                    .as_slice()
            );
        }

        if let Some(exclude_exp) = exclude_exp {
            assert_eq!(
                exclude,
                exclude_exp
                    .iter()
                    .map(|s| format!("{}{}", GLOB_ROOT, s))
                    .collect_vec()
                    .as_slice()
            );
        }
    }

    /// set up a globwalk test in a tempdir, returning the path to the tempdir
    fn setup() -> tempdir::TempDir {
        let tmp = tempdir::TempDir::new("globwalk").unwrap();

        let directories = ["a/b/c", "a/c", "abc", "axbxcxdxe/xxx", "axbxcxdxexxx", "b"];

        let files = [
            "a/abc",
            "a/b/c/d",
            "a/c/b",
            "abc/b",
            "abcd",
            "abcde",
            "abxbbxdbxebxczzx",
            "abxbbxdbxebxczzy",
            "axbxcxdxe/f",
            "axbxcxdxe/xxx/f",
            "axbxcxdxexxx/f",
            "axbxcxdxexxx/fff",
            "a☺b",
            "b/c",
            "c",
            "x",
            "xxx",
            "z",
            "α",
            "abc/【test】.txt",
        ];

        for dir in directories.iter() {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }

        for file in files.iter() {
            std::fs::File::create(tmp.path().join(file)).unwrap();
        }

        #[cfg(unix)]
        {
            // these files/symlinks won't work on Windows
            std::fs::File::create(tmp.path().join("-")).unwrap();
            std::fs::File::create(tmp.path().join("]")).unwrap();

            std::os::unix::fs::symlink("../axbxcxdxe/", tmp.path().join("b/symlink-dir")).unwrap();
            std::os::unix::fs::symlink(
                "/tmp/nonexistant-file-20160902155705",
                tmp.path().join("broken-symlink"),
            )
            .unwrap();
            std::os::unix::fs::symlink("a/b", tmp.path().join("working-symlink")).unwrap();
        }
        tmp
    }

    #[test_case("a*/**", 22, 22 => matches None ; "wildcard followed by doublestar")]
    #[test_case("**/*f", 4, 4 => matches None ; "leading doublestar expansion")]
    #[test_case("**f", 4, 4 => matches None ; "transform leading doublestar")]
    #[test_case("a**", 22, 22 => matches None ; "transform trailing doublestar")]
    #[test_case("abc", 1, 1 => matches None ; "exact match")]
    #[test_case("*", 19, 15 => matches None ; "single star match")]
    #[test_case("*c", 2, 2 => matches None ; "single star suffix match")]
    #[test_case("a*", 9, 9 => matches None ; "single star prefix match")]
    #[test_case("a*/b", 2, 2 => matches None ; "single star prefix with suffix match")]
    #[test_case("a*b*c*d*e*", 3, 3 => matches None ; "multiple single stars match")]
    #[test_case("a*b*c*d*e*/f", 2, 2 => matches None ; "single star and double star match")]
    #[test_case("a*b?c*x", 2, 2 => matches None ; "single star and question mark match")]
    #[test_case("ab[c]", 1, 1 => matches None ; "character class match")]
    #[test_case("ab[b-d]", 1, 1 => matches None ; "character class range match")]
    #[test_case("ab[e-g]", 0, 0 => matches None ; "character class range mismatch")]
    #[test_case("a?b", 1, 1 => matches None ; "question mark unicode match")]
    #[test_case("a[!a]b", 1, 1 => matches None ; "negated character class unicode match 2")]
    #[test_case("a???b", 0, 0 => matches None ; "insufficient question marks mismatch")]
    #[test_case("a[^a][^a][^a]b", 0, 0 => matches None ; "multiple negated character classes mismatch")]
    #[test_case("a?b", 1, 1 => matches None ; "question mark not matching slash")]
    #[test_case("a*b", 1, 1 => matches None ; "single star not matching slash 2")]
    #[test_case("[x-]", 0, 0 => matches Some(WalkError::BadPattern(_, _)) ; "trailing dash in character class fail")]
    #[test_case("[-x]", 0, 0 => matches Some(WalkError::BadPattern(_, _)) ; "leading dash in character class fail")]
    #[test_case("[a-b-d]", 0, 0 => matches Some(WalkError::BadPattern(_, _)) ; "dash within character class range fail")]
    #[test_case("[a-b-x]", 0, 0 => matches Some(WalkError::BadPattern(_, _)) ; "dash within character class range fail 2")]
    #[test_case("[", 0, 0 => matches Some(WalkError::BadPattern(_, _)) ; "unclosed character class error")]
    #[test_case("[^", 0, 0 => matches Some(WalkError::BadPattern(_, _)) ; "unclosed negated character class error")]
    #[test_case("[^bc", 0, 0 => matches Some(WalkError::BadPattern(_, _)) ; "unclosed negated character class error 2")]
    #[test_case("a[", 0, 0 => matches Some(WalkError::BadPattern(_, _)) ; "unclosed character class error after pattern")]
    #[test_case("ad[", 0, 0 => matches Some(WalkError::BadPattern(_, _)) ; "unclosed character class error after pattern 2")]
    #[test_case("*x", 4, 4 => matches None ; "star pattern match")]
    #[test_case("[abc]", 3, 3 => matches None ; "single character class match")]
    #[test_case("a/**", 7, 7 => matches None ; "a followed by double star match")]
    #[test_case("**/c", 4, 4 => matches None ; "double star and single subdirectory match")]
    #[test_case("a/**/b", 2, 2 => matches None ; "a followed by double star and single subdirectory match")]
    #[test_case("a/**/c", 2, 2 => matches None ; "a followed by double star and multiple subdirectories match 2")]
    #[test_case("a/**/d", 1, 1 => matches None ; "a followed by double star and multiple subdirectories with target match")]
    #[test_case("a/b/c", 1, 1 => matches None ; "a followed by subdirectories and double slash mismatch")]
    #[test_case("ab{c,d}", 1, 1 => matches None ; "pattern with curly braces match")]
    #[test_case("ab{c,d,*}", 5, 5 => matches None ; "pattern with curly braces and wildcard match")]
    #[test_case("ab{c,d}[", 0, 0 => matches Some(WalkError::BadPattern(_, _)))]
    #[test_case("a{,bc}", 0, 0 => matches Some(WalkError::BadPattern(_, _)) ; "a followed by comma or b or c")]
    #[test_case("a/{b/c,c/b}", 2, 2 => matches None)]
    #[test_case("{a/{b,c},abc}", 3, 3 => matches None)]
    #[test_case("{a/ab*}", 1, 1 => matches None)]
    #[test_case("a/*", 3, 3 => matches None)]
    #[test_case("{a/*}", 3, 3 => matches None ; "curly braces with single star match")]
    #[test_case("{a/abc}", 1, 1 => matches None)]
    #[test_case("{a/b,a/c}", 2, 2 => matches None)]
    #[test_case("abc/**", 3, 3 => matches None ; "abc then doublestar")]
    #[test_case("**/abc", 2, 2 => matches None)]
    #[test_case("**/*.txt", 1, 1 => matches None)]
    #[test_case("**/【*", 1, 1 => matches None)]
    #[test_case("b/**/f", 0, 0 => matches None)]
    fn glob_walk(
        pattern: &str,
        result_count: usize,
        result_count_windows: usize,
    ) -> Option<WalkError> {
        glob_walk_inner(
            pattern,
            if cfg!(windows) {
                result_count_windows
            } else {
                result_count
            },
        )
    }

    // these tests were configured to only run on unix, and not on windows
    #[cfg(unix)]
    // cannot use * as a path token on windows
    #[test_case("a\\*b", 0 => matches None ; "escaped star mismatch")]
    #[test_case("[\\]a]", 2 => matches None ; "escaped bracket match")]
    #[test_case("[\\-]", 1  => matches None; "escaped dash match")]
    #[test_case("[x\\-]", 2  => matches None; "escaped dash in character class match")]
    #[test_case("[\\-x]", 2  => matches None; "escaped dash and character match")]
    // #[test_case("[-]", Some(WalkError::BadPattern("[-]".into())), 0 ; "bare dash in character
    // class match")] #[test_case("[x-]", Some(WalkError::BadPattern("[x-]".into())), 0 ;
    // "trailing dash in character class match 2")] #[test_case("[-x]",
    // Some(WalkError::BadPattern("[-x]".into())), 0 ; "leading dash in character class match 2")]
    // #[test_case("[a-b-d]", Some(WalkError::BadPattern("[a-b-d]".into())), 0 ; "dash within
    // character class range match 3")] #[test_case("\\",
    // Some(WalkError::BadPattern("\\".into())), 0 ; "single backslash error")]
    #[test_case("a/\\**", 0  => matches None; "a followed by escaped double star and subdirectories mismatch")]
    #[test_case("a/\\[*\\]", 0  => matches None; "a followed by escaped character class and pattern mismatch")]
    // in the go implementation, broken-symlink is yielded,
    // however in symlink mode, walkdir yields broken symlinks as errors.
    // Note that walkdir _always_ follows root symlinks. We handle this in the layer
    // above wax.
    #[test_case("broken-symlink", 1 => matches None ; "broken symlinks should be yielded")]
    // globs that match across a symlink should not follow the symlink
    #[test_case("working-symlink/c/*", 0 => matches None ; "working symlink should not be followed")]
    #[test_case("working-sym*/*", 0 => matches None ; "working symlink should not be followed 2")]
    fn glob_walk_unix(pattern: &str, result_count: usize) -> Option<WalkError> {
        glob_walk_inner(pattern, result_count)
    }

    fn glob_walk_inner(pattern: &str, result_count: usize) -> Option<WalkError> {
        let dir = setup();

        let path = AbsoluteSystemPathBuf::try_from(dir.path()).unwrap();
        let success = match super::globwalk(&path, &[pattern.into()], &[], crate::WalkType::All) {
            Ok(e) => e.into_iter(),
            Err(e) => return Some(e),
        };

        assert_eq!(
            success.len(),
            result_count,
            "{}: expected {} matches, but got {:#?}",
            pattern,
            result_count,
            success
        );

        None
    }

    #[test_case(
        &["/test.txt"],
        "/",
        &["*.txt"],
        &[],
        &["/test.txt"],
        &["/test.txt"]
        ; "hello world"
    )]
    #[test_case(
        &["/test.txt", "/subdir/test.txt", "/other/test.txt"],
        "/",
        &["subdir/test.txt", "test.txt"],
        &[],
        &["/subdir/test.txt", "/test.txt"],
        &["/subdir/test.txt", "/test.txt"]
        ; "bullet files"
    )]
    #[test_case(&[
            "/external/file.txt",
            "/repos/some-app/apps/docs/package.json",
            "/repos/some-app/apps/web/package.json",
            "/repos/some-app/bower_components/readline/package.json",
            "/repos/some-app/examples/package.json",
            "/repos/some-app/node_modules/gulp/bower_components/readline/package.json",
            "/repos/some-app/node_modules/react/package.json",
            "/repos/some-app/package.json",
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
            "/repos/some-app/test/mocks/kitchen-sink/package.json",
            "/repos/some-app/tests/mocks/kitchen-sink/package.json",
        ],
        "/repos/some-app/",
        &["packages/*/package.json", "apps/*/package.json"], &["**/node_modules/", "**/bower_components/", "**/test/", "**/tests/"],
        &[
            "/repos/some-app/apps/docs/package.json",
            "/repos/some-app/apps/web/package.json",
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
        ],
        &[
            "/repos/some-app/apps/docs/package.json",
            "/repos/some-app/apps/web/package.json",
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
        ]
        ; "finding workspace package.json files"
    )]
    #[test_case(&[
            "/external/file.txt",
            "/repos/some-app/apps/docs/package.json",
            "/repos/some-app/apps/web/package.json",
            "/repos/some-app/bower_components/readline/package.json",
            "/repos/some-app/examples/package.json",
            "/repos/some-app/node_modules/gulp/bower_components/readline/package.json",
            "/repos/some-app/node_modules/react/package.json",
            "/repos/some-app/package.json",
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
            "/repos/some-app/test/mocks/spanish-inquisition/package.json",
            "/repos/some-app/tests/mocks/spanish-inquisition/package.json",
        ],
        "/repos/some-app/",
        &["**/package.json"],
        &["**/node_modules/", "**/bower_components/", "**/test/", "**/tests/"],
        &[
            "/repos/some-app/apps/docs/package.json",
            "/repos/some-app/apps/web/package.json",
            "/repos/some-app/examples/package.json",
            "/repos/some-app/package.json",
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
        ],
        &[
            "/repos/some-app/apps/docs/package.json",
            "/repos/some-app/apps/web/package.json",
            "/repos/some-app/examples/package.json",
            "/repos/some-app/package.json",
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
        ]
        ; "excludes unexpected workspace package.json files"
    )]
    #[test_case(&[
            "/external/file.txt",
            "/repos/some-app/apps/docs/package.json",
            "/repos/some-app/apps/web/package.json",
            "/repos/some-app/bower_components/readline/package.json",
            "/repos/some-app/examples/package.json",
            "/repos/some-app/node_modules/gulp/bower_components/readline/package.json",
            "/repos/some-app/node_modules/react/package.json",
            "/repos/some-app/package.json",
            "/repos/some-app/packages/xzibit/package.json",
            "/repos/some-app/packages/xzibit/node_modules/street-legal/package.json",
            "/repos/some-app/packages/xzibit/node_modules/paint-colors/package.json",
            "/repos/some-app/packages/xzibit/packages/yo-dawg/package.json",
            "/repos/some-app/packages/xzibit/packages/yo-dawg/node_modules/meme/package.json",
            "/repos/some-app/packages/xzibit/packages/yo-dawg/node_modules/yo-dawg/package.json",
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
            "/repos/some-app/test/mocks/spanish-inquisition/package.json",
            "/repos/some-app/tests/mocks/spanish-inquisition/package.json",
        ],
        "/repos/some-app/",
        &["packages/**/package.json"],
        &["**/node_modules/", "**/bower_components/", "**/test/", "**/tests/"],
        &[
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
            "/repos/some-app/packages/xzibit/package.json",
            "/repos/some-app/packages/xzibit/packages/yo-dawg/package.json",
        ],
        &[
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
            "/repos/some-app/packages/xzibit/package.json",
            "/repos/some-app/packages/xzibit/packages/yo-dawg/package.json",
        ]
        ; "nested packages work")]
    #[test_case(&[
            "/external/file.txt",
            "/repos/some-app/apps/docs/package.json",
            "/repos/some-app/apps/web/package.json",
            "/repos/some-app/bower_components/readline/package.json",
            "/repos/some-app/examples/package.json",
            "/repos/some-app/node_modules/gulp/bower_components/readline/package.json",
            "/repos/some-app/node_modules/react/package.json",
            "/repos/some-app/package.json",
            "/repos/some-app/packages/xzibit/package.json",
            "/repos/some-app/packages/xzibit/node_modules/street-legal/package.json",
            "/repos/some-app/packages/xzibit/node_modules/paint-colors/package.json",
            "/repos/some-app/packages/xzibit/packages/yo-dawg/package.json",
            "/repos/some-app/packages/xzibit/packages/yo-dawg/node_modules/meme/package.json",
            "/repos/some-app/packages/xzibit/packages/yo-dawg/node_modules/yo-dawg/package.json",
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
            "/repos/some-app/test/mocks/spanish-inquisition/package.json",
            "/repos/some-app/tests/mocks/spanish-inquisition/package.json",
        ],
        "/repos/some-app/",
        &["packages/**/package.json", "tests/mocks/*/package.json"],
        &["**/node_modules/", "**/bower_components/", "**/test/", "**/tests/"],
        &[
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
            "/repos/some-app/packages/xzibit/package.json",
            "/repos/some-app/packages/xzibit/packages/yo-dawg/package.json",
        ],
        &[
            "/repos/some-app/packages/colors/package.json",
            "/repos/some-app/packages/faker/package.json",
            "/repos/some-app/packages/left-pad/package.json",
            "/repos/some-app/packages/xzibit/package.json",
            "/repos/some-app/packages/xzibit/packages/yo-dawg/package.json",
        ]
        ; "includes do not override excludes")]
    #[test_case(&[
            "/external/file.txt",
            "/repos/some-app/src/index.js",
            "/repos/some-app/public/src/css/index.css",
            "/repos/some-app/.turbo/turbo-build.log",
            "/repos/some-app/.turbo/somebody-touched-this-file-into-existence.txt",
            "/repos/some-app/.next/log.txt",
            "/repos/some-app/.next/cache/db6a76a62043520e7aaadd0bb2104e78.txt",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
            "/repos/some-app/public/dist/css/index.css",
            "/repos/some-app/public/dist/images/rick_astley.jpg",
        ],
        "/repos/some-app/",
        &[".turbo/turbo-build.log", "dist/**", ".next/**", "public/dist/**"],
        &[],
        &[
            "/repos/some-app/.next",
            "/repos/some-app/.next/cache",
            "/repos/some-app/.next/cache/db6a76a62043520e7aaadd0bb2104e78.txt",
            "/repos/some-app/.next/log.txt",
            "/repos/some-app/.turbo/turbo-build.log",
            "/repos/some-app/dist",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules",
            "/repos/some-app/dist/js/node_modules/browserify.js",
            "/repos/some-app/public/dist",
            "/repos/some-app/public/dist/css",
            "/repos/some-app/public/dist/css/index.css",
            "/repos/some-app/public/dist/images",
            "/repos/some-app/public/dist/images/rick_astley.jpg",
        ],
        &[
            "/repos/some-app/.next/cache/db6a76a62043520e7aaadd0bb2104e78.txt",
            "/repos/some-app/.next/log.txt",
            "/repos/some-app/.turbo/turbo-build.log",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
            "/repos/some-app/public/dist/css/index.css",
            "/repos/some-app/public/dist/images/rick_astley.jpg",
        ]
        ; "output globbing grabs the desired content"
    )]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ], "/repos/some-app/",
        &["dist/**"],
        &[],
        &[
            "/repos/some-app/dist",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        &[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ]
        ; "passing ** captures all children")]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        "/repos/some-app/",
        &["dist"],
        &[],
        &["/repos/some-app/dist"],
        &[]
        ; "passing just a directory captures no children")]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ], "/repos/some-app/", &["**/*", "dist/**"], &[ ], &[
            "/repos/some-app/dist",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ], &[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ] ; "redundant includes do not duplicate")]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ], "/repos/some-app/", &["**"], &["**"], &[ ], &[ ] ; "exclude everything, include everything")]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        "/repos/some-app/",
        &["dist/**"],
        &["dist/js"],
        &[
            "/repos/some-app/dist",
            "/repos/some-app/dist/index.html",
        ],
        &[
            "/repos/some-app/dist/index.html",
        ]
        ; "passing just a directory to exclude prevents capture of children")]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        "/repos/some-app/",
        &["dist/**"],
        &["dist/js/**"],
        &[
            "/repos/some-app/dist",
            "/repos/some-app/dist/index.html",
            // "/repos/some-app/dist/js",
        ],
        &["/repos/some-app/dist/index.html",]
        ; "passing ** to exclude prevents capture of children")]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        "/repos/some-app/",
        &["**"],
        &["./"],
        &[],
        &[]
        ; "exclude everything with folder . applies at base path"
    )]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        "/repos/some-app/",
        &["**"],
        &["./dist"],
        &["repos/some-app"],
        &[]
        ; "exclude everything with traversal applies at a non-base path"
    )]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        "/repos/some-app/",
        &["**"],
        &["dist/../"],
        &[],
        &[]
        ; "exclude everything with folder traversal (..) applies at base path"
    )]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js"
        ],
        "/repos/some-app/",
        &["dist/js/../**"],
        &[],
        &[
            "/repos/some-app/dist",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules",
            "/repos/some-app/dist/js/node_modules/browserify.js"],
        &[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ]
        ; "traversal works within base path"
    )]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        "/repos/some-app/",
        &["dist/./././**"],
        &[],
        &[
            "/repos/some-app/dist",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        &[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ]
        ; "self references work (.)"
    )]
    #[test_case(&[
            "/repos/some-app/package.json",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ], "/repos/some-app/", &["*"], &[ ], &[
            "/repos/some-app/dist",
            "/repos/some-app/package.json",
        ], &["/repos/some-app/package.json"] ; "depth of 1 includes handles folders properly")]
    #[test_case(&[
            "/repos/some-app/package.json",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        "/repos/some-app/",
        &["**"],
        &["dist/*"],
        &[
            "/repos/some-app",
            "/repos/some-app/dist",
            "/repos/some-app/package.json",
        ],
        &["/repos/some-app/package.json"]
        ; "depth of 1 excludes prevents capturing folders")]
    #[test_case(&[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        "/repos/some-app",
        &["dist/**"],
        &[],
        &[
            "/repos/some-app/dist",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        &[
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ]
        ; "No-trailing slash basePath works")]
    #[test_case(&[
            "/repos/some-app/included.txt",
            "/repos/some-app/excluded.txt",
        ], "/repos/some-app", &["*.txt"], &["excluded.txt"], &[
            "/repos/some-app/included.txt",
        ], &[
            "/repos/some-app/included.txt",
        ] ; "exclude single file")]
    #[test_case(&[
            "/repos/some-app/one/included.txt",
            "/repos/some-app/one/two/included.txt",
            "/repos/some-app/one/two/three/included.txt",
            "/repos/some-app/one/excluded.txt",
            "/repos/some-app/one/two/excluded.txt",
            "/repos/some-app/one/two/three/excluded.txt",
        ],
        "/repos/some-app",
        &["**"],
        &["**/excluded.txt"],
        &[
            "/repos/some-app/one/included.txt",
            "/repos/some-app/one/two/included.txt",
            "/repos/some-app/one/two/three/included.txt",
            "/repos/some-app/one",
            "/repos/some-app",
            "/repos/some-app/one/two",
            "/repos/some-app/one/two/three",
        ], &[
            "/repos/some-app/one/included.txt",
            "/repos/some-app/one/two/included.txt",
            "/repos/some-app/one/two/three/included.txt",
        ] ; "exclude nested single file")]
    #[test_case(&[
            "/repos/some-app/one/included.txt",
            "/repos/some-app/one/two/included.txt",
            "/repos/some-app/one/two/three/included.txt",
            "/repos/some-app/one/excluded.txt",
            "/repos/some-app/one/two/excluded.txt",
            "/repos/some-app/one/two/three/excluded.txt",
        ], "/repos/some-app", &["**"], &["**"], &[], &[] ; "exclude everything")]
    #[test_case(&[
            "/repos/some-app/one/included.txt",
            "/repos/some-app/one/two/included.txt",
            "/repos/some-app/one/two/three/included.txt",
            "/repos/some-app/one/excluded.txt",
            "/repos/some-app/one/two/excluded.txt",
            "/repos/some-app/one/two/three/excluded.txt",
        ], "/repos/some-app", &["**"], &["**/"], &[], &[] ; "exclude everything with slash")]
    #[test_case(&[
            "/repos/some-app/foo/bar",
            "/repos/some-app/some-foo/bar",
            "/repos/some-app/included",
        ],
        "/repos/some-app",
        &["**"],
        &["*foo"],
        &[
            "repos/some-app",
            "/repos/some-app/included",
        ],
        &[
            "/repos/some-app/included",
        ]
        ; "exclude everything with leading star"
    )]
    #[test_case(&[
            "/repos/some-app/foo/bar",
            "/repos/some-app/foo-file",
            "/repos/some-app/foo-dir/bar",
            "/repos/some-app/included",
        ],
        "/repos/some-app",
        &["**"],
        &["foo*"],
        &[
            "repos/some-app",
            "/repos/some-app/included",
        ],
        &[
            "/repos/some-app/included",
        ]
        ; "exclude everything with trailing star"
    )]
    fn glob_walk_files(
        files: &[&str],
        base_path: &str,
        include: &[&str],
        exclude: &[&str],
        expected: &[&str],
        expected_files: &[&str],
    ) {
        let dir = setup_files(files);
        let base_path = base_path.trim_start_matches('/');
        let path = AbsoluteSystemPathBuf::try_from(dir.path().join(base_path)).unwrap();
        let include: Vec<_> = include.iter().map(|s| s.to_string()).collect();
        let exclude: Vec<_> = exclude.iter().map(|s| s.to_string()).collect();

        for (walk_type, expected) in [
            (crate::WalkType::Files, expected_files),
            (crate::WalkType::All, expected),
        ] {
            let success = super::globwalk(&path, &include, &exclude, walk_type).unwrap();

            let success = success
                .iter()
                .map(|p| p.as_path().strip_prefix(dir.path()).unwrap().as_str())
                .sorted()
                .collect::<Vec<_>>();

            let expected = expected
                .iter()
                .map(|p| {
                    p.trim_start_matches('/')
                        .replace('/', std::path::MAIN_SEPARATOR_STR)
                })
                .sorted()
                .collect::<Vec<_>>();

            assert_eq!(
                success, expected,
                "\n\n{:?}: expected \n{:#?} but got \n{:#?}",
                walk_type, expected, success
            );
        }
    }

    #[test_case(&[
            "/repos/spanish-inquisition/index.html",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        "/repos/some-app/",
        &["../spanish-inquisition/**", "dist/**"],
        &[],
        &[],
        &[]
        ; "globs and traversal and globs do not cross base path"
    )]
    #[test_case(
        &[
            "/repos/spanish-inquisition/index.html",
            "/repos/some-app/dist/index.html",
            "/repos/some-app/dist/js/index.js",
            "/repos/some-app/dist/js/lib.js",
            "/repos/some-app/dist/js/node_modules/browserify.js",
        ],
        "/repos/some-app/",
        &["**/../../spanish-inquisition/**"],
        &[],
        &[],
        &[]
        ; "globs and traversal and globs do not cross base path doublestart up"
    )]
    fn glob_walk_err(
        files: &[&str],
        _base_path: &str,
        _include: &[&str],
        _exclude: &[&str],
        _expected: &[&str],
        _expected_files: &[&str],
    ) {
        let _dir = setup_files(files);
        // TODO: this test needs to be implemented...
    }

    fn setup_files(files: &[&str]) -> tempdir::TempDir {
        let tmp = tempdir::TempDir::new("globwalk").unwrap();
        for file in files {
            let file = file.trim_start_matches('/');
            let path = tmp.path().join(file);
            let parent = path.parent().unwrap();
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|_| panic!("failed to create {:?}", parent));
            std::fs::File::create(path).unwrap();
        }
        tmp
    }

    #[test]
    fn test_directory_traversal() {
        let files = &["root-file", "child/some-file"];
        let tmp = setup_files(files);
        let root = AbsoluteSystemPathBuf::try_from(tmp.path()).unwrap();
        let child = root.join_component("child");
        let include = &["../*-file".to_string()];
        let exclude = &[];
        let iter = globwalk(&child, include, exclude, WalkType::Files)
            .unwrap()
            .into_iter();
        let results = iter
            .map(|entry| root.anchor(entry).unwrap().to_string())
            .collect::<Vec<_>>();
        let expected = vec!["root-file".to_string()];
        assert_eq!(results, expected);
    }

    #[test]
    fn workspace_globbing() {
        let files = &[
            "package.json",
            "docs/package.json",
            "apps/some-app/package.json",
            "apps/ignored/package.json",
            "node_modules/dep/package.json",
            "apps/some-app/node_modules/dep/package.json",
        ];
        let tmp = setup_files(files);
        let root = AbsoluteSystemPathBuf::try_from(tmp.path()).unwrap();
        let include = &[
            "apps/*/package.json".to_string(),
            "docs/package.json".to_string(),
            "empty/*/package.json".to_string(),
        ];
        let exclude = &["apps/ignored".to_string(), "**/node_modules/**".to_string()];
        let iter = globwalk(&root, include, exclude, WalkType::Files).unwrap();
        let paths = iter
            .into_iter()
            .map(|path| {
                let relative = root.anchor(path).unwrap();
                relative.to_string()
            })
            .collect::<HashSet<_>>();
        let expected: HashSet<String> = HashSet::from_iter([
            "docs/package.json"
                .replace('/', std::path::MAIN_SEPARATOR_STR)
                .to_string(),
            "apps/some-app/package.json"
                .replace('/', std::path::MAIN_SEPARATOR_STR)
                .to_string(),
        ]);
        assert_eq!(paths, expected);
    }
}
