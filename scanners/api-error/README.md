# API error

## Samples

```json
{
  "subtype": "api_error",
  "version": "2.1.185",
  "type": "system",
  "timestamp": "2026-06-22T12:46:58.460Z",
  "gitBranch": "HEAD",
  "maxRetries": 10,
  "uuid": "cb2c5db5-0d6b-4833-b807-5e0d53531397",
  "entrypoint": "claude-vscode",
  "level": "error",
  "isSidechain": false,
  "userType": "external",
  "cwd": "/home/garrett/Code/gage",
  "error": {
    "message": "Connection error.",
    "isNetworkDown": false,
    "formatted": "Unable to connect to API (ECONNRESET)",
    "rateLimits": null,
    "connection": {
      "code": "ECONNRESET",
      "isSSLError": false,
      "message": "The socket connection was closed unexpectedly. For more information, pass `verbose: true` in the second argument to fetch()"
    }
  },
  "parentUuid": "e6042f97-2a2d-4d3e-8911-9c254b1a9102",
  "retryInMs": 613.5404306810848,
  "sessionId": "dd0885f9-c194-4f00-8066-281750f24f07",
  "retryAttempt": 1
}
```
