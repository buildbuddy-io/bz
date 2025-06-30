package claude

import (
	"bufio"
	"encoding/json"
	"fmt"
	"log"
	"os"
	"os/exec"

	"github.com/anthropics/anthropic-sdk-go"
)

func Run(stdin *os.File, extraArgs []string, interactive bool) (int, error) {
	claudeArgs := []string{}

	claudeArgs = append(claudeArgs,
		"--verbose",
		"--output-format=stream-json",
		"--print",
		"--dangerously-skip-permissions",
		"--append-system-prompt",
		"You are a Bazel expert and you are helping the user fix a Bazel error. If no workspace is found, you will help the user migrate the project to Bazel.")

	if !interactive {
	}

	claudeArgs = append(claudeArgs, extraArgs...)

	cmd := exec.Command("claude", claudeArgs...)

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return 1, err
	}

	stderr, err := cmd.StderrPipe()
	if err != nil {
		return 1, err
	}

	if stdin != nil {
		cmd.Stdin = stdin
	}

	if err := cmd.Start(); err != nil {
		return 1, err
	}

	// Handle stderr in a goroutine
	go func() {
		scanner := bufio.NewScanner(stderr)
		for scanner.Scan() {
			fmt.Fprintln(os.Stderr, scanner.Text())
		}
	}()

	// Handle stdout
	scanner := bufio.NewScanner(stdout)
	for scanner.Scan() {
		line := scanner.Text()
		var response Message

		if err := json.Unmarshal([]byte(line), &response); err != nil {
			log.Printf("Failed to parse JSON line: %v", err)
			continue
		}

		for _, content := range response.Message.Content {
			if content.OfText != nil {
				fmt.Println(content.OfText.Text)
			}
			if content.OfToolUse != nil {
				fmt.Printf("%s (%s)\n", content.OfToolUse.Name, content.OfToolUse.Input)
			}
		}

		if response.Type == "result" && *response.Result != "" {
			fmt.Print(response.Result)
		}
	}

	if err := scanner.Err(); err != nil {
		log.Printf("Error reading stdout: %v", err)
	}

	if err := cmd.Wait(); err != nil {
		log.Printf("Failed to run claude: %v", err)
	}

	return 0, nil
}

type Message struct {
	// Discriminators
	Type    string  `json:"type"`              // "assistant" | "user" | "result" | "system"
	Subtype *string `json:"subtype,omitempty"` // see schema

	// Always present
	SessionID string `json:"session_id"`

	// assistant / user
	Message anthropic.MessageParam `json:"message,omitempty"` // Message | MessageParam

	// result-* (success or error)
	DurationMs    *float64 `json:"duration_ms,omitempty"`
	DurationAPIMs *float64 `json:"duration_api_ms,omitempty"`
	IsError       *bool    `json:"is_error,omitempty"`
	NumTurns      *int     `json:"num_turns,omitempty"`
	Result        *string  `json:"result,omitempty"` // only for subtype=="success"
	TotalCostUSD  *float64 `json:"total_cost_usd,omitempty"`

	// system-init
	APIKeySource   *string     `json:"apiKeySource,omitempty"`
	Cwd            *string     `json:"cwd,omitempty"`
	Tools          []string    `json:"tools,omitempty"`
	MCPServers     []MCPServer `json:"mcp_servers,omitempty"`
	Model          *string     `json:"model,omitempty"`
	PermissionMode *string     `json:"permissionMode,omitempty"`
}

// MCPServer matches the “mcp_servers” objects in a system-init message.
type MCPServer struct {
	Name   string `json:"name"`
	Status string `json:"status"`
}
