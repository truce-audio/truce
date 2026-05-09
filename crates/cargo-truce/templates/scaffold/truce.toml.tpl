[vendor]
name = "{vendor_name}"
id = "{vendor_id}"
url = "https://example.com"
au_manufacturer = "{vendor_fourcc}"
{{ for p in plugins }}
[[plugin]]
name = "{p.display}"
bundle_id = "{p.bundle_id}"
crate = "{p.crate_name}"
category = "{p.category}"
fourcc = "{p.fourcc}"
au_tag = "{p.au_tag}"
{{ endfor }}{{ if suite }}
# Suite installer — bundles every plugin above into a single
# `.pkg` (macOS), `.exe` (Windows) and `.tar.gz` (Linux) so end
# users install the whole collection in one go. The per-plugin
# installers still ship in parallel; pass `--no-per-plugin` to
# `cargo truce package` to drop them. Add `plugins = [...]` /
# `exclude_plugins = [...]` to scope the suite to a subset, or
# `formats = [...]` to override the default-all.
[[suite]]
name = "{suite.name}"
bundle_id = "{suite.bundle_id}"
{{ endif }}