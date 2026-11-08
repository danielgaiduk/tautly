#!/usr/bin/env bash
# Makes the repository of task go-multi-timeout in the current (empty) directory.
set -euo pipefail
cat > 'go.mod' <<'TAUTLY_EOF'
module example.com/task

go 1.21
TAUTLY_EOF
cat > 'config.go' <<'TAUTLY_EOF'
package task

// Config is how a client is set up.
type Config struct {
	BaseURL string
}

// DefaultConfig is the configuration used when none is given.
func DefaultConfig() Config {
	return Config{BaseURL: "http://localhost"}
}
TAUTLY_EOF
cat > 'client.go' <<'TAUTLY_EOF'
package task

// Client talks to a server.
type Client struct {
	base string
}

// NewClient is a client set up by c.
func NewClient(c Config) *Client {
	return &Client{base: c.BaseURL}
}

// Base is the server's base URL.
func (c *Client) Base() string {
	return c.base
}
TAUTLY_EOF
cat > 'client_test.go' <<'TAUTLY_EOF'
package task

import (
	"testing"
	"time"
)

func TestTimeout(t *testing.T) {
	c := NewClient(DefaultConfig())
	if c.Timeout() != 30*time.Second || c.Base() != "http://localhost" {
		t.Fatalf("default: %v %q", c.Timeout(), c.Base())
	}
	cfg := DefaultConfig()
	cfg.Timeout = time.Second
	if NewClient(cfg).Timeout() != time.Second {
		t.Fatal("given timeout")
	}
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task go-multi-timeout'
