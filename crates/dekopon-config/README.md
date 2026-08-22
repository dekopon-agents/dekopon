# dekopon-config

Source-aware loading and validation for local Dekopon YAML or JSON catalogs.

Discovery supports an explicit path, `DEKOPON_CONFIG`, XDG/HOME configuration, and a project-local `dekopon.yaml`.

Validation scans the whole catalog before it refuses one: duplicates, invalid names, unsupported API versions, and missing or inconsistent references are collected into a single `ConfigError::Invalid` report with one line per problem. Only a failure that makes continuing impossible — an unreadable file or invalid YAML — stops at the first error.
