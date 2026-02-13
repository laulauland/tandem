# Core Beliefs

1. **Thin server, smart client integration with jj**
   - Server provides storage + head coordination + notifications.
   - Workflow semantics remain jj-native.

2. **Stock jj behavior first**
   - Tandem should feel like jj, not a new VCS.

3. **Remote store, local working copy**
   - Fast local file operations with shared global history.

4. **No stale-head workflow**
   - Always read latest heads from server.

5. **Compatibility over invention**
   - Reuse jj protobuf/object formats where possible.
   - Keep server repo a normal jj+git colocated repo.

6. **Integration tests drive slices**
   - Every major claim in SPEC should be proven by an integration test.
