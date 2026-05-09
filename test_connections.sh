#!/bin/bash

# Test script to reproduce the connection issue

cd /Users/franklee/Documents/misc/bluefin

# Build binaries
echo "Building binaries..."
cargo build --release --bin server --bin client 2>&1 | grep -E "(Compiling|Finished)"

# Start server in background
echo "Starting server..."
./target/release/server > /tmp/server_output.txt 2>&1 &
SERVER_PID=$!
echo "Server PID: $SERVER_PID"

# Wait for server to start
sleep 2

# Run client
echo "Running client..."
timeout 30 ./target/release/client > /tmp/client_output.txt 2>&1 &
CLIENT_PID=$!
echo "Client PID: $CLIENT_PID"

# Wait a bit for connections
sleep 5

# Check outputs
echo ""
echo "=== SERVER OUTPUT ==="
cat /tmp/server_output.txt | tail -20
echo ""
echo "=== CLIENT OUTPUT ==="
cat /tmp/client_output.txt | tail -20

# Cleanup
echo ""
echo "Cleaning up..."
kill $SERVER_PID $CLIENT_PID 2>/dev/null
wait 2>/dev/null

echo "Test complete"
