# @kurisu/deepmerge build dependency

Private local fork of the bundled CommonJS distribution from deepmerge 4.3.1, under its MIT license.

The local patch defines destination properties directly in both object merge paths. This preserves the returned object's prototype when copying dictionary keys and addresses GHSA-gcqx-w48m-w2h4. The original version remains unchanged. The private package name identifies the patched fork without claiming an upstream fixed release.

The file override keeps the Svelte Vite plugin's `deepmerge` import while resolving it to `@kurisu/deepmerge` for build configuration. Remove this override when an upstream fixed release is available.

Source: https://github.com/TehShrike/deepmerge/tree/v4.3.1
Advisory: https://github.com/advisories/GHSA-gcqx-w48m-w2h4
