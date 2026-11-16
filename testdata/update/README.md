Throwaway signing key and signatures for `src/update` tests only. The
secret key was discarded; these files can't sign anything else, and the
test key is never trusted by a build (release keys live in
`packaging/release-keys.pub`).
