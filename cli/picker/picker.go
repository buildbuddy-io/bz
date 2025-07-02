package picker

import (
	"fmt"
	"strings"

	"github.com/charmbracelet/bubbles/help"
	"github.com/charmbracelet/bubbles/key"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

type Option struct {
	Label string
	Value string
}

type keyMap struct {
	Up     key.Binding
	Down   key.Binding
	Select key.Binding
	Quit   key.Binding
}

func (k keyMap) ShortHelp() []key.Binding {
	return []key.Binding{k.Up, k.Down, k.Select, k.Quit}
}

func (k keyMap) FullHelp() [][]key.Binding {
	return [][]key.Binding{
		{k.Up, k.Down},
		{k.Select, k.Quit},
	}
}

var keys = keyMap{
	Up: key.NewBinding(
		key.WithKeys("up", "k"),
		key.WithHelp("↑/k", "up"),
	),
	Down: key.NewBinding(
		key.WithKeys("down", "j"),
		key.WithHelp("↓/j", "down"),
	),
	Select: key.NewBinding(
		key.WithKeys("enter"),
		key.WithHelp("enter", "select"),
	),
	Quit: key.NewBinding(
		key.WithKeys("q", "ctrl+c", "esc"),
		key.WithHelp("q/esc", "quit"),
	),
}

type model struct {
	prompt   string
	options  []Option
	cursor   int
	selected string
	help     help.Model
	keys     keyMap
}

func initialModel(prompt string, options []Option) model {
	return model{
		prompt:  prompt,
		options: options,
		cursor:  0,
		help:    help.New(),
		keys:    keys,
	}
}

func (m model) Init() tea.Cmd {
	return nil
}

func (m model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.KeyMsg:
		switch {
		case key.Matches(msg, m.keys.Quit):
			return m, tea.Quit
		case key.Matches(msg, m.keys.Up):
			if m.cursor > 0 {
				m.cursor--
			}
		case key.Matches(msg, m.keys.Down):
			if m.cursor < len(m.options)-1 {
				m.cursor++
			}
		case key.Matches(msg, m.keys.Select):
			m.selected = m.options[m.cursor].Value
			return m, tea.Quit
		}
	}
	return m, nil
}

func (m model) View() string {
	s := strings.Builder{}
	s.WriteString("\n")
	s.WriteString(lipgloss.NewStyle().Bold(true).Render(m.prompt))
	s.WriteString("\n\n")

	for i, opt := range m.options {
		cursor := " [  ] "
		if m.cursor == i {
			cursor = " [✅] "
			s.WriteString(lipgloss.NewStyle().Foreground(lipgloss.Color("14")).Render(cursor + opt.Label))
		} else {
			s.WriteString(cursor + opt.Label)
		}
		s.WriteString("\n")
	}

	s.WriteString("\n")
	s.WriteString(m.help.View(m.keys))

	return s.String()
}

func ShowPicker(prompt string, options []Option) (string, error) {
	p := tea.NewProgram(initialModel(prompt, options))
	m, err := p.Run()
	if err != nil {
		return "", err
	}

	if finalModel, ok := m.(model); ok {
		if finalModel.selected == "" {
			return "", fmt.Errorf("")
		}
		return finalModel.selected, nil
	}

	return "", fmt.Errorf("unexpected model type")
}
