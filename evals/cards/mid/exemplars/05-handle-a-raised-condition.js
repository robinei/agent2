// The two differ only in backoff, and the larger one is the safer
// default under load — that is a judgement I can make from what is
// already here, so it does not need to go to a person.
tell("backup-2 has the longer backoff (500ms vs 200ms) — safer under load, so I'm keeping that one.");
return resume("backup-2.txt");