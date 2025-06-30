package please

import (
	"flag"
	"os"
	"strings"

	"bz.build/cli/claude"
)

var (
	flags = flag.NewFlagSet("ask", flag.ContinueOnError)
)

var (
	usage = `
usage: bz ` + flags.Name() + `

Asks bz to perform a task.
`
)

func HandleAsk(args []string) (int, error) {

	claudePrompt := strings.Join(args, " ")

	claude.Run(os.Stdin, []string{claudePrompt}, true)

	return 0, nil
}
