/* Package version: 0.1.0 */

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

typedef struct {
  uint64_t bar;
} Foo;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

void doit(const Foo*);

#ifdef __cplusplus
} // extern "C"
#endif // __cplusplus
