#!/usr/bin/env python3

import gevent
import gevent.wsgi

from gevent import monkey
from werkzeug.exceptions import (BadRequest, HTTPException,
                                 InternalServerError, NotFound)
from werkzeug.routing import Map, Rule, RequestRedirect
from werkzeug.wrappers import Request, Response
from werkzeug.wsgi import responder


class ScriptHandler(object):
    def __init__(self):
        self.router = Map([
            Rule('/script', methods=['GET'], endpoint='script')
        ])

    @responder
    def __call__(self, environ, start_response):
        try:
            return self.wsgi_app(environ, start_response)
        except:
            logger.error(traceback.format_exc())
            return InternalServerError()

    def wsgi_app(self, environ, start_response):
        route = self.router.bind_to_environ(environ)
        try:
            endpoint, args = route.match()
        except RequestRedirect as e:
            return e
        except HTTPException:
            return NotFound()

        request = Request(environ)
        print(str(request.args))
        response = Response()
        response.mimetype = 'text/plain'
        response.status_code = 200
        response.data = 'OK'
        return response

if __name__ == '__main__':
    server = gevent.wsgi.WSGIServer(('0.0.0.0', 8080), ScriptHandler())
    gevent.spawn(server.serve_forever).join()
    
