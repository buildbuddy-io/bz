package claude

import (
	"bufio"
	"encoding/json"
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"time"

	"bz.build/cli/picker"
	// "bz.build/cli/spinner" // Uncomment when spinner code is enabled
	"bz.build/cli/textarea"
	"golang.org/x/term"
)

func Run(stdin *os.File, extraArgs []string, interactive bool) (int, error) {
	claudeArgs := []string{}

	renderThinking()
	// todo add gemini and openai and amp support

	systemPrompt := "You are a Bazel expert and you are helping the user fix a Bazel error. " +
		"If no workspace is found, you will help the user migrate the project to Bazel. " +
		"If the fix is not straightforward, think of 3 possible fixes and present them to the user using the <select><option>...</option></select> syntax. " +
		"If asking the user a yes/no question, use the <select><option>...</option></select> syntax. "

	claudeArgs = append(claudeArgs,
		"--verbose",
		"--output-format=stream-json",
		"--print",
		"--dangerously-skip-permissions",
		"--append-system-prompt",
		systemPrompt)

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

	// Create ~/.bz directory if it doesn't exist
	homeDir, err := os.UserHomeDir()
	if err != nil {
		return 1, fmt.Errorf("failed to get home directory: %v", err)
	}
	bzDir := filepath.Join(homeDir, ".bz")
	if err := os.MkdirAll(bzDir, 0755); err != nil {
		return 1, fmt.Errorf("failed to create .bz directory: %v", err)
	}

	// Create output file for stream-json
	outputFile, err := os.Create(filepath.Join(bzDir, "output.json"))
	if err != nil {
		return 1, fmt.Errorf("failed to create output file: %v", err)
	}
	defer outputFile.Close()

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
	toolUseLines := make(map[string]int) // Map tool use IDs to line numbers
	currentNumLines := 0

	for scanner.Scan() {
		line := scanner.Text()
		var response LogLine

		// Write raw output to file
		fmt.Fprintln(outputFile, line)

		// fmt.Printf("line: %s\n", line)

		if err := json.Unmarshal([]byte(line), &response); err != nil {
			log.Printf("Failed to parse JSON line: %v", err)
			continue
		}

		// json, err := json.Marshal(response)
		// if err != nil {
		// 	log.Printf("Failed to marshal content: %v", err)
		// 	continue
		// }
		// fmt.Printf("⏺ %s\n", string(json))

		if response.Message != nil {
			for _, content := range response.Message.Content {
				renderDone()
				if content.Name != "" {
					bullet, numLines := renderBullet(renderToolUse(content), "  ", "\033[1m⏺\033[0m ", true)
					fmt.Printf("%s", bullet)
					toolUseLines[content.ID] = currentNumLines // Store line count for this tool use
					currentNumLines += numLines
				}
				if content.Text != "" {
					text := regexp.MustCompile(`<select>((?s).*?)</select>`).ReplaceAllString(content.Text, "")
					bullet, numLines := renderBullet(text, "  ", "\033[1m⏺\033[0m ", true)
					fmt.Printf("%s", bullet)
					currentNumLines += numLines

					// Check for select/option tags in the text
					if matches := regexp.MustCompile(`<select>((?s).*?)</select>`).FindAllStringSubmatch(content.Text, -1); matches != nil {
						for _, match := range matches {
							selectContent := match[1]
							options := []picker.Option{}

							// Extract options with attributes - match anything between <option and > for attributes
							optionMatches := regexp.MustCompile(`<option([^>]*)>([^<]+)</option>`).FindAllStringSubmatch(selectContent, -1)
							for _, opt := range optionMatches {
								label := opt[2]

								options = append(options, picker.Option{
									Label: label,
									Value: label,
								})
							}

							options = append(options, picker.Option{
								Label: "Something else",
								Value: "Something else",
							})

							if len(options) > 0 {
								// Show picker and get selection
								selected, err := picker.ShowPicker("Which would you like to do?", options)
								if err != nil {
									continue
								}

								if selected == "Something else" {
									// Get custom input from user
									userInput, err := textarea.ShowTextarea("What would you like to do instead?", "Type here... For example: "+options[0].Label)
									if err != nil {
										continue
									}
									if userInput == "" {
										continue
									}
									selected = userInput
								}

								Run(stdin, []string{"--continue", selected}, interactive)
							}
						}
					}
					// todo tab rendering of long text
				}
				if content.Content != "" {
					if toolLine, ok := toolUseLines[content.ToolUseID]; ok {
						if content.IsError {
							fmt.Printf("%s", renderColoredBullet(currentNumLines-toolLine-1, "red", "⏺", ""))
						} else {
							fmt.Printf("%s", renderColoredBullet(currentNumLines-toolLine-1, "green", "⏺", ""))
						}
					}
				}
			}
		}

		renderThinking()
	}

	renderDone()

	if err := scanner.Err(); err != nil {
		log.Printf("Error reading stdout: %v", err)
	}

	if err := cmd.Wait(); err != nil {
		log.Printf("Failed to run claude: %v", err)
	}

	return 0, nil
}

func renderPath(path string) string {
	cwd, err := os.Getwd()
	if err != nil {
		return path
	}
	rel, err := filepath.Rel(cwd, path)
	if err != nil {
		return path
	}
	return rel
}

func renderToolUse(content Part) string {
	inputJSON, err := content.Input.MarshalJSON()
	if err != nil {
		log.Printf("Failed to marshal input: %v", err)
		return ""
	}

	var jsonMap map[string]interface{}
	if err := json.Unmarshal(inputJSON, &jsonMap); err != nil {
		log.Printf("Failed to unmarshal input: %v", err)
		return ""
	}

	inputs := ""

	if content.Name == "LS" {
		content.Name = "List"
		inputs = fmt.Sprintf("(%s)", renderPath(jsonMap["path"].(string)))
	} else if content.Name == "Grep" {
		content.Name = "Find"
	} else if content.Name == "TodoWrite" {
		content.Name = "Update Todos"
		var todoItems []string
		// jsonMap["todos"] contains the array of todo items
		if todosArray, ok := jsonMap["todos"].([]interface{}); ok {
			for _, todo := range todosArray {
				todoMap, ok := todo.(map[string]interface{})
				if !ok {
					log.Printf("Failed to unmarshal todo: %v", todo)
					continue
				}
				content := todoMap["content"].(string)
				status := todoMap["status"].(string)

				item := content
				if status == "completed" {
					item = fmt.Sprintf("☒ \033[9m%s\033[0m", item)
				} else {
					item = fmt.Sprintf("☐ %s", item)
				}
				todoItems = append(todoItems, item)
			}
		} else {
			log.Printf("Failed to get todos array from jsonMap: %v", jsonMap)
		}
		inputs = fmt.Sprintf("\n%s", strings.Join(todoItems, "\n"))
	} else if content.Name == "Read" {
		inputs = fmt.Sprintf("(%s)", renderPath(jsonMap["file_path"].(string)))
	} else if content.Name == "Bash" {
		inputs = fmt.Sprintf("(%s)", jsonMap["command"].(string))
	}

	if inputs == "" {
		var values []string
		for _, v := range jsonMap {
			values = append(values, fmt.Sprintf("%v", v))
		}
		inputs = fmt.Sprintf("(%s)", strings.Join(values, ", "))
	}

	return fmt.Sprintf("\033[1m%s\033[0m%s", content.Name, inputs)
}

func renderBullet(text string, indent string, bulletPrefix string, newLine bool) (string, int) {
	width := 80 // Default width
	if w, _, err := term.GetSize(int(os.Stdout.Fd())); err == nil {
		width = w
	}

	// Split text into words
	words := strings.Fields(text)
	if len(words) == 0 {
		return "", 0
	}

	// First line gets the bullet with 2 space indent
	// bulletPrefix := "⏺ "
	// indent := "  "
	lineWidth := width - len(indent)

	var result strings.Builder
	var currentLine strings.Builder
	currentLine.WriteString(bulletPrefix)
	lineLen := len(bulletPrefix)

	// Count number of lines
	numLines := 1
	if newLine {
		result.WriteString("\n")
		numLines++
	}

	// Build lines word by word
	for _, word := range words {
		if lineLen+len(word)+1 > lineWidth && currentLine.Len() > len(bulletPrefix) {
			// Line would be too long, start a new one
			result.WriteString(currentLine.String())
			result.WriteString("\n")
			currentLine.Reset()
			currentLine.WriteString(indent)
			lineLen = len(indent)
			numLines++
		}
		currentLine.WriteString(" ")
		lineLen++
		currentLine.WriteString(word)
		lineLen += len(word)
	}

	// Add final line
	if currentLine.Len() > 0 {
		result.WriteString(currentLine.String())
	}
	result.WriteString("\n")

	return result.String(), numLines
}

func renderColoredBullet(height int, color string, bullet string, suffix string) string {
	// ANSI escape codes
	greenColor := "\033[32m"
	redColor := "\033[31m"
	blueColor := "\033[96m"
	resetColor := "\033[0m"

	if color == "green" {
		color = greenColor
	} else if color == "red" {
		color = redColor
	} else if color == "blue" {
		color = blueColor
	}

	// Move up N lines, back to start, replace bullet, then move back down N lines
	return fmt.Sprintf("\033[s\033[%dA\r%s%s%s%s\033[u", height, color, bullet, resetColor, suffix)
}

var isThinking = false
var stopThinking = make(chan bool)

func renderThinking() {
	if isThinking || !term.IsTerminal(int(os.Stdout.Fd())) {
		return
	}

	isThinking = true
	fmt.Print("\n\r\033[36m⣿\033[0m  Thinking...\n\n")

	go func() {
		spinChars := []rune{'⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'}
		i := 0
		for {
			select {
			case <-stopThinking:
				return
			case <-time.After(66 * time.Millisecond):
				numDots := (i / 10) % 4
				dots := strings.Repeat(".", numDots)
				fmt.Printf("%s", renderColoredBullet(2, "blue", fmt.Sprintf("%c", spinChars[i%len(spinChars)]), fmt.Sprintf("  Thinking%s   ", dots)))
				i = (i + 1)
			}
		}
	}()
}

func renderDone() {
	if !term.IsTerminal(int(os.Stdout.Fd())) {
		return
	}

	stopThinking <- true
	fmt.Print("\033[1A\033[K\033[1A\033[K\033[1A\033[K")
	isThinking = false
}

type LogLine struct {
	Type           string   `json:"type"`              // "system", "assistant", "user", "result", …
	Subtype        string   `json:"subtype,omitempty"` // e.g. "init" on system rows
	Cwd            string   `json:"cwd,omitempty"`
	SessionID      string   `json:"session_id,omitempty"`
	Tools          []string `json:"tools,omitempty"`
	MCPServers     []string `json:"mcp_servers,omitempty"`
	Model          string   `json:"model,omitempty"`
	PermissionMode string   `json:"permissionMode,omitempty"`
	APIKeySource   string   `json:"apiKeySource,omitempty"`

	// Rows produced by the assistant / user embed a Message object.
	Message         *Message `json:"message,omitempty"`
	ParentToolUseID *string  `json:"parent_tool_use_id,omitempty"`
}

// Message mirrors the structure under `"message": { … }`.
type Message struct {
	ID    string `json:"id,omitempty"`
	Type  string `json:"type"` // "message"
	Role  string `json:"role,omitempty"`
	Model string `json:"model,omitempty"`

	Content      []Part  `json:"content,omitempty"`
	StopReason   *string `json:"stop_reason,omitempty"`
	StopSequence *string `json:"stop_sequence,omitempty"`
	Usage        *Usage  `json:"usage,omitempty"`
}

// Part is one element of the `"content": […]` array.
// It copes with "text", "tool_use", "tool_result", etc.
type Part struct {
	Type string `json:"type"`           // "text", "tool_use", "tool_result", …
	Text string `json:"text,omitempty"` // for plain text parts

	// Tool-related fields (only present on tool_* parts)
	ID        string          `json:"id,omitempty"`
	Name      string          `json:"name,omitempty"`
	Input     json.RawMessage `json:"input,omitempty"` // arbitrary JSON payload
	ToolUseID string          `json:"tool_use_id,omitempty"`
	Content   string          `json:"content,omitempty"` // tool_result payload
	IsError   bool            `json:"is_error,omitempty"`
}

// Usage captures the token accounting blob at the tail of each assistant message.
type Usage struct {
	InputTokens              int    `json:"input_tokens,omitempty"`
	CacheCreationInputTokens int    `json:"cache_creation_input_tokens,omitempty"`
	CacheReadInputTokens     int    `json:"cache_read_input_tokens,omitempty"`
	OutputTokens             int    `json:"output_tokens,omitempty"`
	ServiceTier              string `json:"service_tier,omitempty"`
}
