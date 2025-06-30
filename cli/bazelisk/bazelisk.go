package bazelisk

import (
	"fmt"
	"io"
	goLog "log"
	"os"
	"sync"

	"github.com/bazelbuild/bazelisk/config"
	"github.com/bazelbuild/bazelisk/core"
	"github.com/bazelbuild/bazelisk/repositories"
	"github.com/creack/pty"
	"github.com/mattn/go-isatty"
)

var (
	setVersionOnce sync.Once
	setVersionErr  error
)

const (
	// bazelisk environment variable name that skips tools/bazel if set to a
	// non-empty string.
	skipWrapperEnvVar = "BAZELISK_SKIP_WRAPPER"
)

type RunOpts struct {
	// Stdout is the Writer where bazelisk should write its stdout.
	// Defaults to os.Stdout if nil.
	Stdout io.Writer

	// Stderr is the Writer where bazelisk should write its stderr.
	// Defaults to os.Stderr if nil.
	Stderr io.Writer

	// SkipWrapper skips the tools/bazel wrapper if it exists.
	SkipWrapper bool
}

func Run(args []string, opts *RunOpts) (exitCode int, err error) {
	if opts.SkipWrapper {
		prev, ok := os.LookupEnv(skipWrapperEnvVar)
		if err := os.Setenv(skipWrapperEnvVar, "true"); err != nil {
			return -1, fmt.Errorf("failed to set %s: %s", skipWrapperEnvVar, err)
		}
		// Reset BAZELISK_SKIP_WRAPPER to its previous state after running
		// bazelisk.
		if ok {
			defer os.Setenv(skipWrapperEnvVar, prev)
		} else {
			defer os.Unsetenv(skipWrapperEnvVar)
		}
	}

	repos := createRepositories(core.MakeDefaultConfig())

	if opts.Stdout != nil || opts.Stderr != nil {
		var errRedirect **os.File
		if opts.Stdout == opts.Stderr {
			errRedirect = &os.Stderr
			close, err := redirectStdio(opts.Stdout, &os.Stdout, errRedirect)
			if err != nil {
				return -1, err
			}
			defer close()
		} else {
			if opts.Stdout != nil {
				close, err := redirectStdio(opts.Stdout, &os.Stdout)
				if err != nil {
					return -1, err
				}
				defer close()
			}
			if opts.Stderr != nil {
				errRedirect = &os.Stderr
				close, err := redirectStdio(opts.Stderr, errRedirect)
				if err != nil {
					return -1, err
				}
				defer close()
			}
		}
		if errRedirect != nil {
			// Prevent Bazelisk `log.Printf` call to write directly to stderr
			oldWriter := goLog.Writer()
			goLog.SetOutput(*errRedirect)
			defer goLog.SetOutput(oldWriter)
		}
	}
	return core.RunBazelisk(args, repos)
}

func createRepositories(bazeliskConf config.Config) *core.Repositories {
	gcs := &repositories.GCSRepo{}
	config := core.MakeDefaultConfig()
	gitHub := repositories.CreateGitHubRepo(config.Get("BAZELISK_GITHUB_TOKEN"))
	// Fetch LTS releases & candidates, rolling releases and Bazel-at-commits from GCS, forks from GitHub.
	return core.CreateRepositories(gcs, gitHub, gcs, gcs, true)
}

// Redirects either os.Stdout or os.Stderr to the given writer. Calling the
// returned close function stops redirection.
func redirectStdio(w io.Writer, stdio ...**os.File) (close func(), err error) {
	var closePipe func()
	f, ok := w.(*os.File)
	if !ok {
		pw, c, err := makePipeWriter(w)
		if err != nil {
			return nil, err
		}
		closePipe = c
		f = pw
	}
	original := make([]*os.File, len(stdio))
	for i := range stdio {
		original[i] = *stdio[i]
		*stdio[i] = f
	}
	close = func() {
		for i := range stdio {
			*stdio[i] = original[i]
		}
		if closePipe != nil {
			closePipe()
		}
	}
	return close, nil
}

// makePipeWriter adapts a writer to an *os.File by using an os.Pipe().
// The returned file should not be closed; instead the returned closeFunc
// should be called to ensure that all data from the pipe is flushed to the
// underlying writer.
func makePipeWriter(w io.Writer) (pw *os.File, closeFunc func(), err error) {
	pr, pw, err := os.Pipe()
	if err != nil {
		return
	}
	done := make(chan struct{})
	go func() {
		io.Copy(w, pr)
		close(done)
	}()
	closeFunc = func() {
		pw.Close()
		// Wait until the pipe contents are flushed to w.
		<-done
	}
	return
}

func RunWithLogFile(args []string, logFileName string) (exitCode int, err error) {
	// Create the output file where the original bazel output will be written,
	// for post-bazel plugins to read.
	outputFile, err := os.Create(logFileName)
	if err != nil {
		return 1, err
	}
	defer outputFile.Close()

	isWritingToTerminal := IsTTY(os.Stdout) && IsTTY(os.Stderr)
	w := io.MultiWriter(outputFile, os.Stderr)
	opts := &RunOpts{
		Stdout: os.Stdout,
		Stderr: w,
	}
	if isWritingToTerminal {
		// We're writing to a MultiWriter in order to capture Bazel's output to
		// a file, but also writing output to a terminal. Hint to bazel that we
		// are still writing to a terminal, by setting --color=yes --curses=yes
		// (with lowest priority, in case the user wants to set those flags
		// themselves).
		// args = addTerminalFlags(args)

		ptmx, tty, err := pty.Open()
		if err != nil {
			return 1, fmt.Errorf("failed to allocate pty: %s", err)
		}
		defer func() {
			// 	Close pty/tty (best effort).
			_ = tty.Close()
			_ = ptmx.Close()
		}()
		if err := pty.InheritSize(os.Stdout, tty); err != nil {
			return 1, fmt.Errorf("failed to inherit terminal size: %s", err)
		}
		// Note: we don't listen to resize events (SIGWINCH) and re-inherit the
		// size, because Bazel itself doesn't do that currently. So it wouldn't
		// make a difference either way.
		opts.Stdout = tty
		opts.Stderr = tty
		go io.Copy(w, ptmx)
	}

	return Run(args, opts)
}

// IsTTY returns whether the given file descriptor is connected to a terminal.
func IsTTY(f *os.File) bool {
	return isatty.IsTerminal(f.Fd())
}
