# dekopon-config

Source-aware loading and validation for local Dekopon YAML or JSON catalogs.

Discovery supports an explicit path, `DEKOPON_CONFIG`, XDG/HOME configuration, and a project-local `dekopon.yaml`.

Validation scans the whole catalog before it refuses one: duplicates, invalid names, unsupported API versions, and missing or inconsistent references are collected into a single `ConfigError::Invalid` report with one line per problem. Only a failure that makes continuing impossible — an unreadable file or invalid YAML — stops at the first error.

Skills load here too. `load_skill` reads one directory in the open Agent Skills layout — named after the skill, holding a `SKILL.md` whose YAML front matter carries `name` (which must equal the directory name) and `description`, optionally `license`, `compatibility`, a scalar-valued `metadata` map, and `allowed-tools` (recorded, not enforced), then a Markdown body — plus every supporting file beside it, into a `Skill` whose `SkillResource`s are addressed by `/`-separated relative path. A skill is one authored unit, so it is refused at its first problem with a `SkillError` naming the file: an unknown front-matter key, a symbolic link, a non-UTF-8 file or file name, or anything that is neither a regular file nor a directory. Hidden entries are skipped. Every bound is fixed before a byte is read: `SKILL.md` at most 64 KiB, each resource at most 256 KiB, at most 64 resources and 1 MiB of them per skill, nested at most four directories deep.

The catalog resolves each `spec.skills` path against its own directory (an absolute path stands), loads every skill while it validates, and reports each failure as a `CatalogProblem::Skill` and two directories carrying one name for the same agent as a `CatalogProblem::DuplicateSkill`, in the same report as everything else. `LocalCatalog::agent_skills` returns an agent's skills in the order `spec.skills` names them, read whole, so a session never touches the filesystem to show a model one. Skill text is untrusted model text exactly as `instructions` is: it grants nothing.
