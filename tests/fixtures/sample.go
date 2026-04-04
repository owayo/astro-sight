package main

import (
	"fmt"
	"strings"
)

type Server struct {
	Host string
	Port int
}

func NewServer(host string, port int) *Server {
	return &Server{Host: host, Port: port}
}

func (s *Server) Address() string {
	return fmt.Sprintf("%s:%d", s.Host, s.Port)
}

func (s *Server) Start() error {
	addr := s.Address()
	fmt.Println(strings.ToUpper(addr))
	return nil
}
