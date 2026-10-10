# Offline model/provider metadata

`provider-catalogue.json` is a compact projection of the public
[models.dev API](https://models.dev/api.json), captured on 10 October 2026.
It contains metadata, not provider code, credentials, or live verification claims.
The accompanying models.dev license applies to this data.

Regenerate with `python3 crates/providers/scripts/update_catalogue.py` from the
repository root. The update downloads public metadata only and has a size bound.
Review the resulting diff and run the provider catalogue tests before committing.
