package spinner

import (
	"fmt"
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/spinner"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

type spinnerModel struct {
	spinner  spinner.Model
	quitting bool
	message  string
}

func initialSpinnerModel(message string) spinnerModel {
	s := spinner.New()
	s.Spinner = spinner.Points
	s.Style = lipgloss.NewStyle().Foreground(lipgloss.Color("14"))
	return spinnerModel{spinner: s, message: message}
}

func (m spinnerModel) Init() tea.Cmd {
	return m.spinner.Tick
}

func (m spinnerModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.KeyMsg:
		switch msg.String() {
		case "q", "esc", "ctrl+c":
			m.quitting = true
			return m, tea.Quit
		default:
			return m, nil
		}

	case spinner.TickMsg:
		var cmd tea.Cmd
		m.spinner, cmd = m.spinner.Update(msg)
		return m, cmd

	case stopSpinnerMsg:
		m.quitting = true
		return m, tea.Quit

	default:
		return m, nil
	}
}

func (m spinnerModel) View() string {
	if m.quitting {
		return ""
	}
	str := fmt.Sprintf("%s %s", m.spinner.View(), m.message)
	return str
}

type stopSpinnerMsg struct{}

type Spinner struct {
	program *tea.Program
	stopped bool
}

func NewSpinner(message string) *Spinner {
	return &Spinner{
		program: tea.NewProgram(initialSpinnerModel(message)),
		stopped: false,
	}
}

func (s *Spinner) Start() {
	if s.stopped {
		return
	}
	go func() {
		if _, err := s.program.Run(); err != nil {
			// Silently ignore errors
		}
	}()
	// Give the spinner a moment to start
	time.Sleep(50 * time.Millisecond)
}

func (s *Spinner) Stop() {
	if s.stopped {
		return
	}
	s.stopped = true
	s.program.Send(stopSpinnerMsg{})
	// Clear the spinner line
	fmt.Print("\r" + strings.Repeat(" ", 50) + "\r")
}
