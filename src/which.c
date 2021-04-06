#include "run-command.c"

#ifdef _WIN32
#define winansi_init(...)
#define main cinnabar_main
extern int cinnabar_main(int argc, const char *argv[]);
#include "compat/mingw.c"

char *which(const char *file) {
	return path_lookup(file, 0);
}

#else

char *which(const char *file) {
	return locate_in_PATH(file);
}
#endif
