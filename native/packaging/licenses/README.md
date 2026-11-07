# Supplemental dependency licenses

`build.py` collects license and notice files from the locked Cargo dependencies. Some published crates omit those files. `index.json` supplies texts from their recorded upstream revisions and links each text to its source. These files are copied unchanged.

Three MIT crates ship only a license declaration. Their notices preserve the published authors and include standard MIT permission text without inventing a copyright year. `pathfinder_geometry` supplies a copyright and dual-license header. Its Apache option is reproduced with the Apache text from the same upstream repository.

When updating dependencies, review these version-specific entries against the new package sources. A missing text fails the build. The generated notices also include build dependencies and bundled optional source notices, so they are a conservative inventory rather than a claim that every listed component is linked.
