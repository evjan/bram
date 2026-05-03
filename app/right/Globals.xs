function lastAssistantText(jsonlText) {
  if (!jsonlText) return '';
  const lines = jsonlText.split('\n');
  let last = null;
  for (const line of lines) {
    if (!line) continue;
    try {
      const r = JSON.parse(line);
      if (r.type === 'assistant' && r.message && r.message.content) {
        last = r;
      }
    } catch (e) {}
  }
  if (!last) return '';
  const content = last.message.content;
  if (typeof content === 'string') return content;
  return (Array.isArray(content) ? content : [])
    .filter(c => c && c.type === 'text')
    .map(c => c.text)
    .join('\n\n');
}
