/* SSH stand-in for remote_sync_paths, compiled for the rsync runtime. It only
 * reads synthetic directories; no shell, network or SSH credentials are used. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static void require(int condition, const char *message) {
    if (!condition) {
        fprintf(stderr, "fake SSH: %s\n", message);
        exit(1);
    }
}

int main(int argc, char **argv) {
    int arg = 1;
    while (arg < argc && strncmp(argv[arg], "-o", 2) == 0) {
        arg++;
    }
    require(arg < argc, "missing host");
    const char *host = argv[arg++];
    require(strcmp(host, "devbox") == 0 || strcmp(host, "otherbox") == 0,
            "unexpected host");
    const char *root = getenv("WAKE_TEST_REMOTE_HOME");
    require(root != NULL, "missing synthetic home");
    require(chdir(root) == 0 && chdir(host) == 0, "cannot enter synthetic home");
    require(arg < argc, "missing remote command");

    if (arg == argc - 1 && strncmp(argv[arg], "sh -c ", 6) == 0) {
        require(strstr(argv[arg], ".claude/projects") != NULL, "unexpected probe");
        struct stat st;
        if (stat(".claude/projects", &st) == 0 && S_ISDIR(st.st_mode)) {
            puts(".claude/projects");
        }
        return 0;
    }

    require(strcmp(argv[arg], "rsync") == 0, "unexpected remote command");
    int server = 0, sender = 0;
    for (int i = arg + 1; i < argc; i++) {
        server |= strcmp(argv[i], "--server") == 0;
        sender |= strcmp(argv[i], "--sender") == 0;
    }
    require(server && sender, "only read-only rsync sender is allowed");
    execvp(argv[arg], argv + arg);
    perror("fake SSH: start rsync sender");
    return 1;
}
