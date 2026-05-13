#ifndef BeamBridge_h
#define BeamBridge_h

#include <stdbool.h>

int beam_init(const char* erl_root, const char* eval_expr);
bool beam_is_initialized(void);

#endif
