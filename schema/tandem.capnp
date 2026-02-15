@0xb37e3e4ea24a0dfe;

interface Store {
  getRepoInfo @0 () -> (info :RepoInfo);

  getObject @1 (kind :ObjectKind, id :Data) -> (data :Data);
  putObject @2 (kind :ObjectKind, data :Data) -> (id :Data, normalizedData :Data);

  getOperation @3 (id :Data) -> (data :Data);
  putOperation @4 (data :Data) -> (id :Data);

  getView @5 (id :Data) -> (data :Data);
  putView @6 (data :Data) -> (id :Data);

  resolveOperationIdPrefix @7 (hexPrefix :Text)
    -> (resolution :PrefixResolution, match :Data);

  getHeads @8 () -> (heads :List(Data), version :UInt64,
                     workspaceHeads :List(WorkspaceHead));

  updateOpHeads @9 (
    oldIds :List(Data),
    newId :Data,
    expectedVersion :UInt64,
    workspaceId :Text
  ) -> (ok :Bool, heads :List(Data), version :UInt64,
        workspaceHeads :List(WorkspaceHead));

  watchHeads @10 (watcher :HeadWatcher, afterVersion :UInt64)
    -> (cancel :Cancel);

  getHeadsSnapshot @11 () -> (
    heads :List(Data),
    version :UInt64,
    operations :List(IdBytes),
    views :List(IdBytes)
  );

  getRelatedCopies @12 (copyId :Data) -> (copies :List(Data));
}

interface HeadWatcher {
  notify @0 (version :UInt64, heads :List(Data)) -> ();
}

interface Cancel {
  cancel @0 () -> ();
}

struct WorkspaceHead {
  workspaceId @0 :Text;
  commitId @1 :Data;
}

struct IdBytes {
  id @0 :Data;
  data @1 :Data;
}

enum ObjectKind {
  commit @0;
  tree @1;
  file @2;
  symlink @3;
  copy @4;
}

enum PrefixResolution {
  noMatch @0;
  singleMatch @1;
  ambiguous @2;
}

struct RepoInfo {
  protocolMajor @0 :UInt16;
  protocolMinor @1 :UInt16;
  jjVersion @2 :Text;

  backendName @3 :Text;
  opStoreName @4 :Text;

  commitIdLength @5 :UInt16;
  changeIdLength @6 :UInt16;

  rootCommitId @7 :Data;
  rootChangeId @8 :Data;
  emptyTreeId @9 :Data;
  rootOperationId @10 :Data;

  capabilities @11 :List(Capability);
}

enum Capability {
  watchHeads @0;
  headsSnapshot @1;
  copyTracking @2;
}
