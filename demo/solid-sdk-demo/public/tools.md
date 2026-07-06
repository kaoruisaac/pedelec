# Available App Tools

## get_current_page

Read the current browser page title and URL.

Returns JSON like:

```json
{
  "title": "Pedelec SDK Solid Demo",
  "url": "http://localhost:5173/"
}
```

```bash
pedelec-cli tool-call get_current_page '{}'
```

## get_selected_text

Read the text currently selected on the page.

Returns JSON like:

```json
{
  "text": "selected text"
}
```

```bash
pedelec-cli tool-call get_selected_text '{}'
```

## ask_user

Ask the user a question and wait for their text response. The `question` field is optional; if omitted, the app shows a default question.

Returns JSON like:

```json
{
  "answer": "user response"
}
```

```bash
pedelec-cli tool-call ask_user '{"question":"What should I do next?"}'
```

## throw_error

Trigger the demo error path by throwing an error from the tool handler. This is intended for testing error handling.

```bash
pedelec-cli tool-call throw_error '{}'
```
