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
{{ endfor }}