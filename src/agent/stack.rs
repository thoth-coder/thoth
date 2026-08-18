//! What kind of project this is, and what checks it.
//!
//! One table, not a branch per language: supporting another stack is adding a
//! row here, and nothing anywhere else in thoth names a language or a command.
//!
//! Every row answers two questions that are easy to confuse. What command
//! checks the whole project without running it, and whether running the code
//! already did that check. A compiler reads every line before it emits
//! anything, so for Rust, Go, C or C# the answer is yes and the ordinary
//! "build it and run its tests" rule is enough. A type stripper reads none of
//! them and an interpreter reads only the line it is about to execute, so for
//! TypeScript, JavaScript, PHP, Python or Ruby the answer is no: the server
//! starts, answers every request the test made, and is still broken in every
//! line that did not run. That gap is the whole reason this file exists.

pub struct Stack {
    /// Files whose presence says a project is this kind. A leading `*.` means
    /// any file with that extension in the top level, which is how the .NET
    /// and Visual Studio projects name themselves.
    pub markers: &'static [&'static str],
    pub name: &'static str,
    /// What to run to check the whole project without running it, written to
    /// be read by the model: a model that has to guess this guesses the
    /// command from whichever ecosystem it saw most of in training.
    pub check: &'static str,
    /// Whether running the program already checks all of it.
    pub run_checks: bool,
    /// Why not, as a clause finishing "running it is not that check, ...".
    /// Empty when `run_checks` is true and nobody asks.
    pub because: &'static str,
    /// Extensions this stack owns. Only consulted for stacks that running
    /// does not check, to notice a file that was changed and never looked at.
    pub exts: &'static [&'static str],
    /// Where this ecosystem's api documentation lives, as a bare host, so a
    /// search can be pointed at it with `site:`. thoth's search is eight
    /// results scraped off one page, and a model guessing a phrase gets
    /// tutorials and stale blog posts; naming the host it should scope to is
    /// the cheapest thing that turns that into the reference. Empty when the
    /// ecosystem has no single obvious home.
    pub docs: &'static str,
    /// Whole words in a shell command that mean a check is what ran. Same
    /// note: only the unchecked stacks need it, but every row carries it,
    /// because "what command checks this" is a fact about the stack and not
    /// about the rule that happens to read it today.
    pub ran: &'static [&'static str],
}

/// Ordered: the first match wins where two stacks describe the same files,
/// which is why TypeScript sits above JavaScript.
const STACKS: &[Stack] = &[
    Stack {
        markers: &["tsconfig.json"],
        name: "TypeScript",
        // The runner alternatives used to come first and the model improvised
        // its way to `bun --check`, which parses the file and checks no types
        // at all. Name the command, then name the wrong turn.
        check: "`bunx tsc --noEmit` (`npx tsc --noEmit` where there is no bun), or the \
project's own typecheck script. `bun --check` and `node --check` only parse the file, they \
check no types at all",
        run_checks: false,
        because: "the types are stripped out before the code runs",
        exts: &["ts", "tsx", "mts", "cts", "svelte", "vue", "astro"],
        docs: "www.typescriptlang.org",
        ran: &[
            "tsc",
            "vue-tsc",
            "svelte-check",
            "typecheck",
            "type-check",
            "astro",
        ],
    },
    Stack {
        markers: &["package.json", "jsconfig.json"],
        name: "JavaScript",
        check: "the project's lint script if it has one, otherwise `node --check` on each \
file you changed",
        run_checks: false,
        because: "a file is not even parsed until something imports it",
        exts: &["js", "jsx", "mjs", "cjs"],
        docs: "developer.mozilla.org",
        ran: &["lint", "eslint", "biome", "standard", "--check"],
    },
    Stack {
        markers: &["Cargo.toml"],
        name: "Rust",
        check: "`cargo check` for speed, `cargo clippy` where the project uses it",
        run_checks: true,
        because: "",
        exts: &["rs"],
        docs: "docs.rs",
        ran: &["cargo"],
    },
    Stack {
        markers: &["go.mod"],
        name: "Go",
        check: "`go build ./...` and `go vet ./...`",
        run_checks: true,
        because: "",
        exts: &["go"],
        docs: "pkg.go.dev",
        ran: &["go"],
    },
    Stack {
        markers: &["composer.json"],
        name: "PHP",
        check: "`php -l` on each file you changed, plus the project's phpstan or psalm if \
it has one",
        run_checks: false,
        because: "only the files a request actually reaches are ever parsed",
        exts: &["php"],
        docs: "www.php.net",
        ran: &["-l", "lint", "phpstan", "psalm", "phan"],
    },
    Stack {
        markers: &[
            "pyproject.toml",
            "requirements.txt",
            "setup.py",
            "setup.cfg",
        ],
        name: "Python",
        check: "the project's mypy, ruff or pyright if it has one, otherwise \
`python -m compileall -q .`",
        run_checks: false,
        because: "a name is only looked up when its line runs",
        exts: &["py", "pyi"],
        docs: "docs.python.org",
        ran: &[
            "mypy",
            "ruff",
            "pyright",
            "pylint",
            "flake8",
            "compileall",
            "py_compile",
        ],
    },
    Stack {
        markers: &["Gemfile", "Rakefile", "*.gemspec"],
        name: "Ruby",
        check: "`ruby -c` on each file you changed, plus rubocop if the project uses it",
        run_checks: false,
        because: "a file is only parsed when it is required",
        exts: &["rb", "rake"],
        docs: "rubydoc.info",
        ran: &["-c", "rubocop", "sorbet", "srb"],
    },
    Stack {
        markers: &["*.sln", "*.csproj", "*.fsproj", "global.json"],
        name: "C#",
        check: "`dotnet build`",
        run_checks: true,
        because: "",
        exts: &["cs", "fs"],
        docs: "learn.microsoft.com",
        ran: &["dotnet", "msbuild"],
    },
    Stack {
        // deliberately not `Makefile`: half the Python and Go repositories in
        // the world have one, and naming the wrong language costs a rule that
        // then contradicts the right one
        markers: &[
            "CMakeLists.txt",
            "meson.build",
            "configure.ac",
            "*.c",
            "*.cc",
            "*.cpp",
        ],
        name: "C/C++",
        check: "the project's own build (`cmake --build build`, `make`), not a compiler \
call you assembled yourself",
        run_checks: true,
        because: "",
        exts: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh"],
        docs: "en.cppreference.com",
        ran: &[
            "make", "cmake", "ninja", "meson", "cc", "gcc", "g++", "clang", "clang++",
        ],
    },
    Stack {
        markers: &["pom.xml", "build.gradle", "build.gradle.kts"],
        name: "Java/Kotlin",
        check: "`mvn -q compile` or `./gradlew compileJava`, whichever the project uses",
        run_checks: true,
        because: "",
        exts: &["java", "kt", "kts"],
        docs: "docs.oracle.com",
        ran: &["mvn", "gradle", "gradlew", "javac", "kotlinc"],
    },
    Stack {
        // rockspecs carry the version in the file name, and a plain
        // .luacheckrc is as much a declaration of a Lua project as a manifest
        markers: &["*.rockspec", ".luacheckrc", "luacheck.rc", "stylua.toml"],
        name: "Lua",
        check: "`luac -p` on each file you changed, plus luacheck if the project uses it",
        run_checks: false,
        because: "a chunk is only compiled when it is first required, so a typo                   in a branch nothing took is still there",
        exts: &["lua"],
        docs: "www.lua.org",
        // not "-p": `luac -p` already carries `luac`, and a bare -p is
        // `mkdir -p` far more often than it is a Lua syntax check
        ran: &["luac", "luacheck", "selene"],
    },
];

/// The stacks a directory listing says are here. Pure, so the table can be
/// tested without a directory to point it at.
pub fn detect(entries: &[String]) -> Vec<&'static Stack> {
    let present = |m: &str| match m.strip_prefix("*.") {
        Some(ext) => entries.iter().any(|e| {
            std::path::Path::new(e)
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case(ext))
        }),
        None => entries.iter().any(|e| e == m),
    };
    let mut found: Vec<&Stack> = STACKS
        .iter()
        .filter(|s| s.markers.iter().any(|m| present(m)))
        .collect();
    // every TypeScript project is also a package.json project, and saying
    // "check the JavaScript with eslint" next to "check the types with tsc"
    // spends a line to describe the same files twice
    if found.iter().any(|s| s.name == "TypeScript") {
        found.retain(|s| s.name != "JavaScript");
    }
    found
}

/// The same for the working directory. Fails to an empty list rather than an
/// error: not knowing the stack costs a rule, not the request.
pub fn here() -> Vec<&'static Stack> {
    let Ok(rd) = std::fs::read_dir(".") else {
        return Vec::new();
    };
    let entries: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    detect(&entries)
}

impl Stack {
    /// Whether this stack owns the file at `path`.
    pub fn owns(&self, path: &str) -> bool {
        std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                let e = e.to_ascii_lowercase();
                self.exts.contains(&e.as_str())
            })
    }

    /// Whether a shell command line was this stack's check. Matched on whole
    /// words, so a path that happens to contain the letters does not count as
    /// having run one: `bun run src/tsconfig-tools/build.ts` checks nothing.
    pub fn checked_by(&self, command: &str) -> bool {
        command
            .to_ascii_lowercase()
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .any(|w| self.ran.contains(&w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_marker_file_names_the_stack_and_a_glob_marker_works() {
        let bun = detect(&listing(&["bun.lock", "package.json", "tsconfig.json"]));
        assert_eq!(bun.len(), 1, "typescript supersedes javascript");
        assert_eq!(bun[0].name, "TypeScript");
        assert!(!bun[0].run_checks);

        let plain = detect(&listing(&["package.json", "index.js"]));
        assert_eq!(plain[0].name, "JavaScript");

        // .NET names its project after the project, so the marker is a glob
        let dotnet = detect(&listing(&["Api.csproj", "Program.cs"]));
        assert_eq!(dotnet[0].name, "C#");
        assert!(dotnet[0].run_checks, "the compiler read every line");

        // a repo can hold two, and both are worth naming
        let both = detect(&listing(&["Cargo.toml", "go.mod"]));
        assert_eq!(both.len(), 2);

        assert!(detect(&listing(&["README.md"])).is_empty());
        // a Makefile is not a language. Naming C here would put a rule about
        // cmake in front of a model working on a Python project
        assert!(detect(&listing(&["Makefile", "README.md"])).is_empty());
        assert_eq!(
            detect(&listing(&["Makefile", "main.c"]))[0].name,
            "C/C++",
            "a flat C project names itself with its sources"
        );
    }

    #[test]
    fn a_check_is_told_apart_from_merely_running_the_code() {
        let ts = detect(&listing(&["tsconfig.json"]));
        let ts = ts[0];
        for cmd in [
            "bunx tsc --noEmit",
            "npx tsc -p .",
            "bun run typecheck",
            "npm run type-check",
            "./node_modules/.bin/tsc.exe --noEmit",
            "bunx vue-tsc --noEmit",
            "bunx svelte-check",
        ] {
            assert!(ts.checked_by(cmd), "{cmd}");
        }
        for cmd in [
            "bun index.ts",
            "bun test",
            "curl -s http://localhost:3000/todos",
            // the letters are in the path, the checker is not in the call
            "bun run src/tsconfig-tools/build.ts",
        ] {
            assert!(!ts.checked_by(cmd), "{cmd}");
        }

        // the interpreted stacks turn on a flag, not on a program name: php
        // is the same binary either way
        let php = detect(&listing(&["composer.json"]));
        assert!(php[0].checked_by("php -l src/Router.php"));
        assert!(!php[0].checked_by("php src/Router.php"));

        assert!(ts.owns("src/App.TSX"));
        assert!(!ts.owns("src/app.js"));
    }

    /// The table is read by the prompt and by the end-of-request note, and
    /// both of them assume these hold for every row.
    #[test]
    fn every_row_is_filled_in() {
        for s in STACKS {
            assert!(!s.markers.is_empty(), "{}", s.name);
            assert!(!s.check.is_empty(), "{}", s.name);
            assert!(!s.exts.is_empty(), "{}", s.name);
            assert!(!s.ran.is_empty(), "{}", s.name);
            // the reason is the second half of a sentence the prompt only
            // writes for the stacks that need one
            assert_eq!(s.because.is_empty(), s.run_checks, "{}", s.name);
        }
    }

    /// Lua is an interpreted stack, so it carries the same trap as PHP and
    /// Ruby: the file runs, and a branch nothing took is still wrong.
    #[test]
    fn lua_is_found_and_is_not_checked_by_running_it() {
        let lua = detect(&listing(&["main.lua", "thoth-1.0-1.rockspec"]));
        assert_eq!(lua.len(), 1);
        let lua = lua[0];
        assert_eq!(lua.name, "Lua");
        assert!(!lua.run_checks);
        assert!(!lua.because.is_empty());
        assert!(lua.owns("scripts/init.lua"));
        assert!(!lua.owns("init.rs"));

        assert!(lua.checked_by("luac -p init.lua"));
        assert!(lua.checked_by("luacheck ."));
        assert!(
            !lua.checked_by("lua init.lua"),
            "running it is not the check"
        );
        // the flag on its own must not count, or every mkdir is a check
        assert!(!lua.checked_by("mkdir -p build"));
        assert!(!lua.docs.is_empty());

        // a .luacheckrc alone is enough to say what kind of project this is
        assert_eq!(detect(&listing(&[".luacheckrc"])).len(), 1);
    }
}
