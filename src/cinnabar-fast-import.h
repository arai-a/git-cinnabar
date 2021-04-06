#ifndef CINNABAR_FAST_IMPORT_H
#define CINNABAR_FAST_IMPORT_H

void record_command(struct strbuf *buf);

int maybe_handle_command(const char *command, struct string_list *args);

void *get_object_entry(const unsigned char *sha1);

void store_git_tree(struct strbuf *tree_buf,
                    const struct object_id *reference,
                    struct object_id *result);

void store_git_commit(struct strbuf *commit_buf, struct object_id *result);

void add_head(struct oid_array *heads, const struct object_id *oid);

const struct object_id *ensure_empty_blob();

#endif
