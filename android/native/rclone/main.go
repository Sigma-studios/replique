// A trimmed rclone for Réplique's Android app.
//
// The app only needs rclone as restic's transport to the Storage Box: restic
// runs `rclone serve restic --stdio` against an SFTP remote. Linking every
// backend and command, as the stock rclone binary does, would add tens of
// megabytes to the APK for nothing, so this main imports only the SFTP backend,
// `serve restic`, and two commands useful when debugging on a phone (`version`
// and `lsf`). build-binaries.sh copies this file into the pinned rclone source
// tree, so it builds against rclone's own go.mod and go.sum.
package main

import (
	_ "github.com/rclone/rclone/backend/sftp"
	"github.com/rclone/rclone/cmd"
	_ "github.com/rclone/rclone/cmd/lsf"
	_ "github.com/rclone/rclone/cmd/serve/restic"
	_ "github.com/rclone/rclone/cmd/version"
)

func main() {
	cmd.Main()
}
