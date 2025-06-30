package register

import (
	"sync"

	"bz.build/cli/command"
	"bz.build/cli/please"
)

// Register registers all known cli commands in the structures laid out in
// cli/command. It is meant to be called immediately on CLI
// startup.
//
// This indirection prevents dependency cycles from occurring when, for example,
// an imported package tries to use the parser, which itself needs to know all
// of the cli commands.
var Register = sync.OnceFunc(register)

func register() {
	command.Commands = []*command.Command{
		{
			Name:    "please",
			Help:    "Asks bz to perform a task.",
			Handler: please.HandleAsk,
			Aliases: []string{},
		},
	}
	command.CommandsByName = make(
		map[string]*command.Command,
		len(command.Commands),
	)
	command.Aliases = make(map[string]*command.Command)
	for _, c := range command.Commands {
		command.CommandsByName[c.Name] = c
		for _, alias := range c.Aliases {
			command.Aliases[alias] = c
		}
	}
}
