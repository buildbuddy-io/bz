package version

import (
	_ "embed"
	"fmt"
	"os"

	"bz.build/cli/arg"
	"bz.build/cli/bazelisk"
)

// The bz cli "version" var is generated in this package according to the
// Bazel flag value --//cli/version:cli_version
//
//go:embed version_flag.txt
var cliVersionFlag string

func HandleVersion(args []string) (exitCode int, err error) {
	if arg.ContainsExact(args, "--cli") {
		fmt.Println(cliVersionFlag)
		return 0, nil
	}

	fmt.Printf("bz version: %s\n", cliVersionFlag)

	return runBazelVersion(args)
}

// runBazelVersion forwards the `version` command to bazel (i.e. `bazel version`)
func runBazelVersion(args []string) (int, error) {
	tempDir, err := os.MkdirTemp("", "buildbuddy-cli-*")
	if err != nil {
		return 1, err
	}
	defer func() {
		os.RemoveAll(tempDir)
	}()

	// Add the `version` command back to args before forwarding
	// the command to bazel
	args = append([]string{"version"}, args...)

	return bazelisk.Run(args, &bazelisk.RunOpts{
		Stdout: os.Stdout,
		Stderr: os.Stderr,
	})
}
