package textarea

import (
	"errors"
	"strings"

	"github.com/charmbracelet/bubbles/help"
	"github.com/charmbracelet/bubbles/key"
	"github.com/charmbracelet/bubbles/textarea"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

var (
	ErrCancelled       = errors.New("cancelled")
	ErrUnexpectedModel = errors.New("unexpected model type")
)

type textareaKeyMap struct {
	Submit key.Binding
	Cancel key.Binding
}

func (k textareaKeyMap) ShortHelp() []key.Binding {
	return []key.Binding{k.Submit, k.Cancel}
}

func (k textareaKeyMap) FullHelp() [][]key.Binding {
	return [][]key.Binding{
		{k.Submit, k.Cancel},
	}
}

var textareaKeys = textareaKeyMap{
	Submit: key.NewBinding(
		key.WithKeys("ctrl+d", "enter"),
		key.WithHelp("ctrl+d/enter", "submit"),
	),
	Cancel: key.NewBinding(
		key.WithKeys("esc", "ctrl+c"),
		key.WithHelp("esc/ctrl+c", "cancel"),
	),
}

type textareaModel struct {
	prompt   string
	textarea textarea.Model
	help     help.Model
	keys     textareaKeyMap
	value    string
	err      error
}

func initialTextareaModel(prompt string, placeholder string) textareaModel {
	ta := textarea.New()
	ta.Placeholder = placeholder
	ta.Focus()
	ta.SetWidth(78)
	ta.SetHeight(3)
	ta.ShowLineNumbers = false

	return textareaModel{
		prompt:   prompt,
		textarea: ta,
		help:     help.New(),
		keys:     textareaKeys,
	}
}

func (m textareaModel) Init() tea.Cmd {
	return textarea.Blink
}

func (m textareaModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmds []tea.Cmd
	var cmd tea.Cmd

	switch msg := msg.(type) {
	case tea.KeyMsg:
		switch {
		case key.Matches(msg, m.keys.Cancel):
			m.err = ErrCancelled
			return m, tea.Quit
		case key.Matches(msg, m.keys.Submit):
			m.value = m.textarea.Value()
			m.textarea.Blur()
			return m, tea.Quit
		}
	}

	m.textarea, cmd = m.textarea.Update(msg)
	cmds = append(cmds, cmd)
	return m, tea.Batch(cmds...)
}

func (m textareaModel) View() string {
	s := strings.Builder{}
	s.WriteString("\n")
	s.WriteString(lipgloss.NewStyle().Bold(true).Render(m.prompt))
	s.WriteString("\n\n")
	s.WriteString(m.textarea.View())
	s.WriteString("\n\n")
	s.WriteString(m.help.View(m.keys))
	return s.String()
}

func ShowTextarea(prompt, placeholder string) (string, error) {
	p := tea.NewProgram(initialTextareaModel(prompt, placeholder))
	m, err := p.Run()
	if err != nil {
		return "", err
	}

	if finalModel, ok := m.(textareaModel); ok {
		if finalModel.err != nil {
			return "", finalModel.err
		}
		return strings.TrimSpace(finalModel.value), nil
	}

	return "", ErrUnexpectedModel
}
