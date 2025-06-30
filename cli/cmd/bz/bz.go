package main

import (
	"fmt"
	"os"
	"strings"
	"time"

	"bz.build/cli/arg"
	"bz.build/cli/bazelisk"
	"bz.build/cli/claude"
	"bz.build/cli/command"
	"bz.build/cli/help"
	"bz.build/cli/log"

	"bz.build/cli/command/register"
)

var (
	// These flags configure the cli at large, and don't apply to any specific
	// cli command
	globalCliFlags = map[string]struct{}{
		// Set to print verbose cli logs
		"verbose": {},
	}
)

func main() {
	exitCode, err := run()
	if err != nil {
		log.Fatal(err)
	}
	os.Exit(exitCode)
}

func run() (exitCode int, err error) {
	start := time.Now()
	// Record original arguments so we can show them in the UI.
	originalArgs := append([]string{}, os.Args...)

	args := handleGlobalCliFlags(os.Args[1:])

	log.Debugf("CLI started at %s", start)
	log.Debugf("args[0]: %s", os.Args[0])

	// Register all known cli commands so that we can query or iterate them later.
	register.Register()

	// Handle help command if applicable.
	exitCode, err = help.HandleHelp(args)
	if err != nil || exitCode >= 0 {
		return exitCode, err
	}

	if c := command.GetCommand(args[0]); c != nil {
		// If the first argument is a cli command, trim it from `args`
		args = args[1:]
		return c.Handler(args)
	}

	// If none of the CLI subcommand handlers were triggered, assume we should
	// handle it as a bazel command.
	return handleBazelCommand(start, args, originalArgs)
}

// handleGlobalCliFlags processes global cli args that don't apply to any specific subcommand
// (--verbose, etc.).
// Returns args with all global cli flags removed
func handleGlobalCliFlags(args []string) []string {
	args, residual := arg.SplitExecutableArgs(args)
	for flag := range globalCliFlags {
		var flagVal string
		flagVal, args = arg.Pop(args, flag)

		// Even if flag is not set and flagVal is "", pass to handlers in case
		// they need to configure a default value
		switch flag {
		case "verbose":
			log.Configure(flagVal)
		}
	}
	return arg.JoinExecutableArgs(args, residual)
}

// handleBazelCommand handles a native bazel command (i.e. commands that are
// directly forwarded to bazel, as opposed to bb cli-specific commands)
//
// originalArgs contains the command as originally typed. We pass it as
// EXPLICIT_COMMAND_LINE metadata to the bazel invocation.
func handleBazelCommand(start time.Time, args []string, originalArgs []string) (int, error) {

	//Prepare a dir for temporary files created by this CLI run
	tempDir, err := os.MkdirTemp("", "buildbuddy-cli-*")
	if err != nil {
		return 1, err
	}
	defer func() {
		os.RemoveAll(tempDir)
	}()

	logFileName := tempDir + "/bazel.log"

	exitCode, err := bazelisk.RunWithLogFile(args, logFileName)

	if err != nil {
		return 1, err
	}

	if exitCode != 0 {
		fmt.Print("Would you like help fixing this error? [y/N/i] ")
		var response string
		fmt.Scanln(&response)

		response = strings.ToLower(response)
		if response == "y" || response == "i" {
			outputFile, err := os.Open(logFileName)
			if err != nil {
				return 1, err
			}
			defer outputFile.Close()

			claude.Run(outputFile, []string{}, response == "i")
		}
	}

	return exitCode, nil
}
