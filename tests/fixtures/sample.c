#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    char *data;
    size_t len;
    size_t cap;
} Buffer;

Buffer *buffer_new(size_t capacity) {
    Buffer *buf = malloc(sizeof(Buffer));
    buf->data = malloc(capacity);
    buf->len = 0;
    buf->cap = capacity;
    return buf;
}

void buffer_append(Buffer *buf, const char *str) {
    size_t slen = strlen(str);
    if (buf->len + slen > buf->cap) {
        buf->cap = (buf->len + slen) * 2;
        buf->data = realloc(buf->data, buf->cap);
    }
    memcpy(buf->data + buf->len, str, slen);
    buf->len += slen;
}

void buffer_free(Buffer *buf) {
    free(buf->data);
    free(buf);
}

int main(void) {
    Buffer *buf = buffer_new(64);
    buffer_append(buf, "hello");
    buffer_free(buf);
    return 0;
}
